//! Resource-expression parsers (`provider.service.Type "name" { ... }`,
//! anonymous resources, `read` data sources) and the shared
//! `parse_block_contents` traversal that backs both resources and the
//! map-literal primary.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::eval_value::{EvalValue, ParsedSinkValue, StructuralHandleKind};
use crate::parser::context::{
    ExpressionMode, ParseContext, extract_key_string, first_inner, next_pair,
};
use crate::parser::error::{
    NON_LITERAL_PREVENT_DESTROY_WARNING_MESSAGE, ParseError, ParseWarning, ParseWarningSpan,
    WarningKind,
};
use crate::parser::util::expression_is_plain_string_literal;
use crate::parser::{BoundDeferredTarget, Rule};
use crate::parser::{
    ParsedDataSource as DataSource, ParsedResource as Resource, ParsedValue as Value,
};
use crate::parser::{parse_expression, parse_expression_eval, parse_expression_in_sink};
use crate::resource::{
    ConcreteValue, DeferredValue, Directives, LoopElementOrigin, LoopProvenance, ParsedIgnorePath,
    ParsedIgnorePathSegment, ResourceId, ResourceIdentity,
};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};

pub(in crate::parser) fn parse_anonymous_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<Resource, ParseError> {
    parse_resource_for_target(
        pair,
        ctx,
        ResourceParseTarget::Pending { origin: None },
        preprocessed_file,
        source_file,
    )
}

pub(crate) enum ResourceParseTarget {
    Pending { origin: Option<LoopElementOrigin> },
    Bound { binding: String },
    DeferredForTemplate { target: BoundDeferredTarget },
}

pub(crate) fn parse_resource_for_target(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    target: ResourceParseTarget,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<Resource, ParseError> {
    let context = match pair.as_rule() {
        Rule::anonymous_resource => "anonymous resource",
        _ => "resource expression",
    };
    let mut iter = pair.into_inner();
    let namespaced_type = next_pair(&mut iter, "resource type", context)?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let provisional_id = match &target {
        ResourceParseTarget::Pending { .. } => {
            ResourceId::pending_with_provider(provider, resource_type.clone(), None)
        }
        ResourceParseTarget::Bound { binding } => ResourceId::with_provider_identity(
            provider,
            resource_type.clone(),
            ResourceIdentity::new(binding.clone()),
            None,
        ),
        ResourceParseTarget::DeferredForTemplate { target } => {
            target.template_resource_id(provider, &resource_type, None)
        }
    };
    let mode = ExpressionMode::resource(provisional_id.clone(), source_file, preprocessed_file);
    let mut quoted_out: Option<HashSet<String>> = Some(HashSet::new());
    let parsed_contents = parse_block_contents_with_quoted(
        iter,
        ctx,
        &mode,
        &mut quoted_out,
        ReservedAttributeNamePolicy::Reject,
    )?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();
    let attributes = parsed_contents.attributes;
    let directives = parsed_contents.directives.unwrap_or_default();

    let (id, binding, loop_provenance) = match target {
        ResourceParseTarget::Pending { origin } => (
            ResourceId::pending_with_provider(
                provider,
                resource_type,
                directives.provider_instance.clone(),
            ),
            None,
            origin.map(LoopProvenance::direct).unwrap_or_default(),
        ),
        ResourceParseTarget::Bound { binding } => (
            ResourceId::with_provider_identity(
                provider,
                resource_type,
                ResourceIdentity::new(binding.clone()),
                directives.provider_instance.clone(),
            ),
            Some(binding),
            LoopProvenance::default(),
        ),
        ResourceParseTarget::DeferredForTemplate { target } => (
            target.template_resource_id(
                provider,
                &resource_type,
                directives.provider_instance.clone(),
            ),
            Some(target.template_address().to_string()),
            LoopProvenance::default(),
        ),
    };
    ctx.remap_binding_use_resource(&provisional_id, &id);

    Ok(Resource {
        id,
        attributes: attributes.into_iter().collect(),
        directives,
        resolved_ignore_changes: Default::default(),
        prefixes: HashMap::new(),
        binding,
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        default_tag_keys: Default::default(),
        loop_provenance,
        quoted_string_attrs,
    })
}

pub(crate) fn parse_block_contents_with_mode(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<Option<IndexMap<String, Value>>, ParseError> {
    let contents = parse_block_contents_with_quoted(
        pairs,
        ctx,
        mode,
        &mut None,
        ReservedAttributeNamePolicy::Allow,
    )?;
    Ok((!contents.pending).then_some(contents.attributes))
}

struct ParsedBlockContents {
    attributes: IndexMap<String, Value>,
    directives: Option<Directives>,
    pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::parser) enum ReservedAttributeNamePolicy {
    Allow,
    Reject,
}

/// If `quoted_out` is `Some`, populate it
/// with the names of top-level attributes whose value is a plain quoted
/// string literal (`attr = "..."`). Used by resource-level callers to
/// build `Resource.quoted_string_attrs` for enum-attribute diagnostics
/// (#2094 / #2229) without re-walking the pest tree.
fn parse_block_contents_with_quoted(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
    quoted_out: &mut Option<HashSet<String>>,
    reserved_attribute_names: ReservedAttributeNamePolicy,
) -> Result<ParsedBlockContents, ParseError> {
    // `IndexMap` so the order in which the user wrote attributes in the
    // .crn file flows all the way to `Resource.attributes` and to
    // `Value::Concrete(ConcreteValue::Map)` payloads — anything that re-renders attributes
    // (formatter, plan display, diagnostics) sees a stable order.
    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();
    let mut authored_nested_blocks: HashMap<String, usize> = HashMap::new();
    let mut directives = None;

    // Local scope extends the parent context with block-scoped let bindings
    let mut local_ctx = ctx.child_scope();
    let mut pending = false;
    let mut pending_names = HashSet::new();

    for content_pair in pairs {
        match content_pair.as_rule() {
            Rule::block_content => {
                let inner = first_inner(content_pair, "block content item", "block content")?;
                match inner.as_rule() {
                    Rule::local_binding => {
                        let mut binding_inner = inner.into_inner();
                        let name =
                            next_pair(&mut binding_inner, "binding name", "local let binding")?
                                .as_str()
                                .to_string();
                        let value = parse_expression(
                            next_pair(&mut binding_inner, "binding value", "local let binding")?,
                            &local_ctx,
                        )?;
                        // Add to local scope only, not to attributes
                        local_ctx.set_variable(name, value);
                    }
                    Rule::attribute => {
                        let mut attr_inner = inner.into_inner();
                        let key_pair =
                            next_pair(&mut attr_inner, "attribute name", "block content")?;
                        let key_span = key_pair.as_span();
                        let key = extract_key_string(key_pair)?;
                        reject_reserved_resource_attribute(
                            &key,
                            key_span,
                            reserved_attribute_names,
                        )?;
                        let value_pair =
                            next_pair(&mut attr_inner, "attribute value", "block content")?;
                        record_quoted_if_literal(quoted_out, &key, &value_pair);
                        let value_mode = mode.at_named_child(key.clone());
                        match parse_expression_in_sink(value_pair, &local_ctx, &value_mode)? {
                            ParsedSinkValue::Ready(value) => {
                                attributes.insert(key, value);
                            }
                            ParsedSinkValue::Pending(_) => {
                                pending = true;
                                pending_names.insert(key);
                            }
                        }
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name = next_pair(&mut block_inner, "block name", "nested block")?
                            .as_str()
                            .to_string();
                        if block_name == "directives" {
                            check_directives_depends_on_elements(block_inner.clone())?;
                            check_directives_provider_value(block_inner.clone())?;
                            directives = Some(parse_directives(block_inner, &local_ctx)?);
                            continue;
                        }
                        // Recursively parse nested block contents (supports arbitrary depth)
                        let authored_count = authored_nested_blocks
                            .entry(block_name.clone())
                            .or_default();
                        let block_index = *authored_count;
                        *authored_count += 1;
                        let block_mode = mode
                            .at_named_child(block_name.clone())
                            .at_list_index(block_index);
                        let block_contents = parse_block_contents_with_quoted(
                            block_inner,
                            &local_ctx,
                            &block_mode,
                            &mut None,
                            ReservedAttributeNamePolicy::Allow,
                        )?;
                        if block_contents.pending {
                            pending = true;
                            pending_names.insert(block_name);
                            continue;
                        }
                        let block_attrs = block_contents.attributes;

                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Concrete(ConcreteValue::Map(block_attrs)));
                    }
                    _ => {}
                }
            }
            Rule::attribute => {
                let mut attr_inner = content_pair.into_inner();
                let key_pair = next_pair(&mut attr_inner, "attribute name", "block content")?;
                let key_span = key_pair.as_span();
                let key = extract_key_string(key_pair)?;
                reject_reserved_resource_attribute(&key, key_span, reserved_attribute_names)?;
                let value_pair = next_pair(&mut attr_inner, "attribute value", "block content")?;
                record_quoted_if_literal(quoted_out, &key, &value_pair);
                let value_mode = mode.at_named_child(key.clone());
                match parse_expression_in_sink(value_pair, &local_ctx, &value_mode)? {
                    ParsedSinkValue::Ready(value) => {
                        attributes.insert(key, value);
                    }
                    ParsedSinkValue::Pending(_) => {
                        pending = true;
                        pending_names.insert(key);
                    }
                }
            }
            _ => {}
        }
    }

    // Convert nested blocks to list attributes
    for (name, blocks) in nested_blocks {
        if !pending_names.contains(&name) {
            attributes.insert(name, Value::Concrete(ConcreteValue::List(blocks)));
        }
    }
    attributes.retain(|name, _| !pending_names.contains(name));

    Ok(ParsedBlockContents {
        attributes,
        directives,
        pending,
    })
}

fn reject_reserved_resource_attribute(
    key: &str,
    span: pest::Span<'_>,
    reserved_attribute_names: ReservedAttributeNamePolicy,
) -> Result<(), ParseError> {
    if reserved_attribute_names == ReservedAttributeNamePolicy::Reject && key.starts_with('_') {
        let (line, column) = span.start_pos().line_col();
        let (end_line, end_column) = span.end_pos().line_col();
        return Err(ParseError::ReservedResourceAttribute {
            attribute: key.to_string(),
            line,
            column,
            end_line,
            end_column,
        });
    }
    Ok(())
}

/// If `quoted_out` is enabled and `value_pair` is a plain quoted string
/// literal (no interpolation, no operators, no list / map wrapping),
/// record `key` in the output set.
fn record_quoted_if_literal(
    quoted_out: &mut Option<HashSet<String>>,
    key: &str,
    value_pair: &pest::iterators::Pair<Rule>,
) {
    if let Some(set) = quoted_out.as_mut()
        && expression_is_plain_string_literal(value_pair.clone())
    {
        set.insert(key.to_string());
    }
}

pub(crate) fn parse_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<Resource, ParseError> {
    parse_resource_for_target(
        pair,
        ctx,
        ResourceParseTarget::Bound {
            binding: binding_name.to_string(),
        },
        preprocessed_file,
        source_file,
    )
}

/// Parse a read resource expression (data source): read aws.s3_bucket { ... }
pub(crate) fn parse_read_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<DataSource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "read resource expression")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let provisional_id = ResourceId::with_provider_identity(
        provider,
        resource_type.clone(),
        ResourceIdentity::new(binding_name.to_string()),
        None,
    );
    let mode = ExpressionMode::resource(provisional_id.clone(), source_file, preprocessed_file);
    let mut quoted_out: Option<HashSet<String>> = Some(HashSet::new());
    let parsed_contents = parse_block_contents_with_quoted(
        inner,
        ctx,
        &mode,
        &mut quoted_out,
        ReservedAttributeNamePolicy::Reject,
    )?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();
    let attributes = parsed_contents.attributes;
    let directives = parsed_contents.directives.unwrap_or_default();

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    let id = ResourceId::with_provider_identity(
        provider,
        resource_type,
        ResourceIdentity::new(resource_name),
        directives.provider_instance.clone(),
    );
    ctx.remap_binding_use_resource(&provisional_id, &id);

    Ok(DataSource {
        id,
        attributes: attributes.into_iter().collect(),
        directives,
        binding: Some(binding_name.to_string()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        quoted_string_attrs,
    })
}

fn parse_directives(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
) -> Result<Directives, ParseError> {
    let mut directives = Directives::default();

    for content_pair in pairs {
        let attr = match content_pair.as_rule() {
            Rule::block_content => {
                first_inner(content_pair, "block content item", "directives block")?
            }
            Rule::attribute => content_pair,
            _ => continue,
        };
        if attr.as_rule() != Rule::attribute {
            continue;
        }

        let mut attr_inner = attr.into_inner();
        let key = next_pair(&mut attr_inner, "attribute name", "directives block")?
            .as_str()
            .to_string();
        let value_pair = next_pair(&mut attr_inner, "attribute value", "directives block")?;
        let line = value_pair.line_col().0;

        match key.as_str() {
            "ignore_changes" => {
                let primary =
                    crate::parser::util::unwrap_to_primary(value_pair).ok_or_else(|| {
                        ParseError::InvalidExpression {
                            line,
                            message:
                                "directives.ignore_changes: must be a list of bare attribute paths"
                                    .to_string(),
                        }
                    })?;
                let list = first_inner(primary, "list", "directives.ignore_changes")?;
                if list.as_rule() != Rule::list {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message:
                            "directives.ignore_changes: must be a list of bare attribute paths"
                                .to_string(),
                    });
                }
                directives.ignore_changes = list
                    .into_inner()
                    .map(|element| parse_ignore_change_path(element, ctx))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            "depends_on" => {
                let primary =
                    crate::parser::util::unwrap_to_primary(value_pair).ok_or_else(|| {
                        ParseError::InvalidExpression {
                            line,
                            message: "directives.depends_on: must be a list of binding identifiers"
                                .to_string(),
                        }
                    })?;
                let list = first_inner(primary, "list", "directives.depends_on")?;
                if list.as_rule() != Rule::list {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: "directives.depends_on: must be a list of binding identifiers"
                            .to_string(),
                    });
                }
                let mut names = Vec::new();
                for element in list.into_inner() {
                    let element_line = element.line_col().0;
                    let eval = parse_expression_eval(element, ctx)?;
                    names.push(eval_as_binding_name(
                        eval,
                        "depends_on: list elements",
                        element_line,
                        BindingNameUse::Dependency,
                    )?);
                }
                directives.depends_on = names;
            }
            "provider" => {
                let eval = parse_expression_eval(value_pair, ctx)?;
                directives.provider_instance = Some(eval_as_binding_name(
                    eval,
                    "provider: value",
                    line,
                    BindingNameUse::Provider,
                )?);
            }
            "force_delete" => {
                directives.force_delete = matches!(
                    parse_expression(value_pair, ctx)?,
                    Value::Concrete(ConcreteValue::Bool(true))
                );
            }
            "create_before_destroy" => {
                directives.create_before_destroy = matches!(
                    parse_expression(value_pair, ctx)?,
                    Value::Concrete(ConcreteValue::Bool(true))
                );
            }
            "prevent_destroy" => {
                let literal = crate::parser::util::unwrap_to_primary(value_pair.clone())
                    .and_then(|primary| primary.into_inner().next())
                    .and_then(|value| match value.as_rule() {
                        Rule::boolean => match value.as_str() {
                            "true" => Some(true),
                            "false" => Some(false),
                            _ => None,
                        },
                        _ => None,
                    });
                let warning_span = {
                    let span = value_pair.as_span();
                    let (start_line, start_column) = span.start_pos().line_col();
                    let (end_line, end_column) = span.end_pos().line_col();
                    ParseWarningSpan {
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                    }
                };
                let parsed = parse_expression(value_pair, ctx)?;
                match literal {
                    Some(value) => directives.set_prevent_destroy(value),
                    None => {
                        if matches!(parsed, Value::Concrete(ConcreteValue::Bool(true))) {
                            directives.set_prevent_destroy(true);
                        } else {
                            ctx.push_warning(ParseWarning {
                                file: None,
                                line,
                                kind: WarningKind::NonLiteralPreventDestroy,
                                span: Some(warning_span),
                                message: NON_LITERAL_PREVENT_DESTROY_WARNING_MESSAGE.to_string(),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(directives)
}

fn parse_ignore_change_path(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ParsedIgnorePath, ParseError> {
    let line = pair.line_col().0;
    let authored = pair.as_str().to_string();
    let primary = crate::parser::util::unwrap_to_primary(pair)
        .ok_or_else(|| invalid_ignore_change_path(line, &authored))?;
    let path = first_inner(primary, "attribute path", "directives.ignore_changes")?;
    let mut segments = Vec::new();

    match path.as_rule() {
        Rule::namespaced_id => push_ignore_path_head(path.as_str(), &mut segments),
        Rule::variable_ref | Rule::subscripted_id => {
            let mut parts = path.into_inner();
            let head = next_pair(
                &mut parts,
                "attribute path head",
                "directives.ignore_changes",
            )?;
            push_ignore_path_head(head.as_str(), &mut segments);
            for access in parts {
                match access.as_rule() {
                    Rule::field_access => {
                        let field =
                            first_inner(access, "field name", "directives.ignore_changes path")?;
                        segments.push(ParsedIgnorePathSegment::Attribute(
                            field.as_str().to_string(),
                        ));
                    }
                    Rule::index_access => {
                        let index =
                            first_inner(access, "subscript", "directives.ignore_changes path")?;
                        match parse_expression(index, ctx)? {
                            Value::Concrete(ConcreteValue::String(key)) => segments.push(
                                ParsedIgnorePathSegment::MapKey(key.expose_plaintext().to_string()),
                            ),
                            Value::Concrete(ConcreteValue::Int(index)) if index >= 0 => {
                                let index = usize::try_from(index)
                                    .map_err(|_| invalid_ignore_change_path(line, &authored))?;
                                segments.push(ParsedIgnorePathSegment::ListIndex(index));
                            }
                            _ => return Err(invalid_ignore_change_path(line, &authored)),
                        }
                    }
                    _ => return Err(invalid_ignore_change_path(line, &authored)),
                }
            }
        }
        _ => return Err(invalid_ignore_change_path(line, &authored)),
    }

    Ok(ParsedIgnorePath::new(segments))
}

fn push_ignore_path_head(head: &str, segments: &mut Vec<ParsedIgnorePathSegment>) {
    segments.extend(
        head.split('.')
            .map(|name| ParsedIgnorePathSegment::Attribute(name.to_string())),
    );
}

fn invalid_ignore_change_path(line: usize, authored: &str) -> ParseError {
    ParseError::InvalidExpression {
        line,
        message: format!(
            "directives.ignore_changes: `{authored}` must be a bare attribute path, not a string literal or computed expression"
        ),
    }
}

#[derive(Clone, Copy)]
enum BindingNameUse {
    Dependency,
    Provider,
}

fn eval_as_binding_name(
    eval: EvalValue,
    context: &str,
    line: usize,
    usage: BindingNameUse,
) -> Result<String, ParseError> {
    match eval {
        EvalValue::StructuralHandle(handle) => {
            let accepted = match usage {
                BindingNameUse::Dependency => handle.kind().is_dependency_target(),
                BindingNameUse::Provider => {
                    matches!(handle.kind(), StructuralHandleKind::Provider)
                }
            };
            if accepted {
                Ok(handle.canonical_binding_name().to_string())
            } else {
                let binding = handle
                    .alias_name()
                    .unwrap_or_else(|| handle.canonical_binding_name());
                let description = handle.kind().description();
                let message = match usage {
                    BindingNameUse::Dependency => format!(
                        "directives.depends_on: '{binding}' is {description} and cannot be used as a dependency target"
                    ),
                    BindingNameUse::Provider => format!(
                        "directives.provider: '{binding}' is {description}, not a provider instance"
                    ),
                };
                Err(ParseError::InvalidExpression { line, message })
            }
        }
        EvalValue::User(Value::Deferred(DeferredValue::Placeholder(
            crate::resource::ParserPlaceholder::ModuleArgument { name, .. },
        ))) if matches!(usage, BindingNameUse::Dependency) => Ok(name),
        EvalValue::User(Value::Deferred(DeferredValue::Placeholder(
            crate::resource::ParserPlaceholder::ModuleArgument { name, .. },
        ))) => Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "directives.provider: module parameter '{name}' cannot be used as a provider instance"
            ),
        }),
        EvalValue::User(Value::Deferred(DeferredValue::BindingRef { binding })) => Ok(binding),
        EvalValue::ProvisionalReference { binding } => Ok(binding),
        EvalValue::User(Value::Concrete(ConcreteValue::EnumIdentifier(name))) => {
            Ok(name.to_string())
        }
        EvalValue::User(Value::Deferred(DeferredValue::ResourceRef { path })) => {
            Err(ParseError::InvalidExpression {
                line,
                message: format!(
                    "directives.{context} must be a bare binding identifier, not an attribute \
                     selector (got `{}`)",
                    path.binding(),
                ),
            })
        }
        other => Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "directives.{context} must be a binding identifier, got {}",
                crate::parser::util::eval_type_name(&other),
            ),
        }),
    }
}

/// Walk the contents of a `directives { ... }` block at the pest layer and
/// reject any `depends_on = [...]` element that is a string literal — bare
/// binding identifiers and string literals collapse to the same `Value`
/// later, so this is the only place we can tell `[role]` from `["role"]`.
fn check_directives_depends_on_elements(
    directives_inner: pest::iterators::Pairs<Rule>,
) -> Result<(), ParseError> {
    for content_pair in directives_inner {
        let attr = match content_pair.as_rule() {
            Rule::block_content => {
                first_inner(content_pair, "block content item", "block content")?
            }
            Rule::attribute => content_pair,
            _ => continue,
        };
        if attr.as_rule() != Rule::attribute {
            continue;
        }
        let mut attr_inner = attr.into_inner();
        let key_pair = next_pair(&mut attr_inner, "attribute name", "directives attribute")?;
        if key_pair.as_str() != "depends_on" {
            continue;
        }
        let value_pair = next_pair(&mut attr_inner, "attribute value", "directives attribute")?;
        let primary = match crate::parser::util::unwrap_to_primary(value_pair) {
            Some(p) => p,
            None => continue,
        };
        let inner = match primary.into_inner().next() {
            Some(p) => p,
            None => continue,
        };
        if inner.as_rule() != Rule::list {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: "directives.depends_on: must be a list of binding identifiers".to_string(),
            });
        }
        for elem in inner.into_inner() {
            if expression_is_plain_string_literal(elem) {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: "directives.depends_on: list elements must be \
                              binding identifiers, not string literals (e.g., \
                              `[role, key]`, not `[\"role\"]`)"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Reject `provider = "<string>"` at the pest layer, where a quoted literal
/// remains distinguishable from a bare binding identifier.
fn check_directives_provider_value(
    directives_inner: pest::iterators::Pairs<Rule>,
) -> Result<(), ParseError> {
    for content_pair in directives_inner {
        let attr = match content_pair.as_rule() {
            Rule::block_content => {
                first_inner(content_pair, "block content item", "block content")?
            }
            Rule::attribute => content_pair,
            _ => continue,
        };
        if attr.as_rule() != Rule::attribute {
            continue;
        }
        let mut attr_inner = attr.into_inner();
        let key_pair = next_pair(&mut attr_inner, "attribute name", "directives attribute")?;
        if key_pair.as_str() != "provider" {
            continue;
        }
        let value_pair = next_pair(&mut attr_inner, "attribute value", "directives attribute")?;
        if expression_is_plain_string_literal(value_pair) {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: "directives.provider: value must be a bare binding identifier, \
                          not a string literal (e.g., `provider = us`, not \
                          `provider = \"us\"`)"
                    .to_string(),
            });
        }
    }
    Ok(())
}
