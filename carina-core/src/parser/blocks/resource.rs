//! Resource-expression parsers (`provider.service.Type "name" { ... }`,
//! anonymous resources, `read` data sources) and the shared
//! `parse_block_contents` traversal that backs both resources and the
//! map-literal primary.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::eval_value::{EvalValue, ParsedSinkValue, UnexpandedStructuralHandleKind};
use crate::parser::context::{
    ExpressionMode, ParseContext, extract_key_string, first_inner, next_pair,
};
use crate::parser::error::ParseError;
use crate::parser::resource_body::{
    BlockContent, BlockContents, DirectiveEntry, ResourceBody, ResourceDirectives,
};
use crate::parser::util::expression_is_plain_string_literal;
use crate::parser::{BoundDeferredTarget, Rule};
use crate::parser::{
    ParsedDataSource as DataSource, ParsedResource as Resource, ParsedValue as Value,
};
use crate::parser::{parse_expression, parse_expression_eval, parse_expression_in_sink};
use crate::resource::{
    AuthoredConcreteValue as ConcreteValue, AuthoredDeferredValue as DeferredValue,
    AuthoredDirectives as Directives, DiagnosticSourceLocations, DirectiveExpr, LoopElementOrigin,
    LoopProvenance, ParsedIgnorePath, ParsedIgnorePathSegment, RelativeBindingName,
    ResourceDirective, ResourceId, ResourceIdentity, UnexpandedDependencyFields,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

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
    Pending {
        origin: Option<LoopElementOrigin>,
    },
    Bound {
        binding: String,
    },
    DeferredForTemplate {
        target: BoundDeferredTarget<RelativeBindingName>,
    },
}

pub(crate) fn parse_resource_for_target(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    target: ResourceParseTarget,
    preprocessed_file: &crate::parser::PreprocessedFile,
    source_file: &std::path::Path,
) -> Result<Resource, ParseError> {
    let declaration =
        crate::parser::DeclarationProvenance::from_pair(&pair, preprocessed_file, source_file);
    let syntax = pair.clone();
    let (resource_type, contents) = ResourceBody::from_pair(pair)
        .expect("resource parser requires a resource body")
        .into_parts();
    let namespaced_type = resource_type.as_str().to_string();

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
        contents,
        ctx,
        &mode,
        &mut quoted_out,
        ReservedAttributeNamePolicy::Reject,
    )?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();
    let authored_slots = ctx.authored_resource_slots(
        &syntax,
        &parsed_contents.sinks,
        &parsed_contents.attributes,
        crate::schema::SchemaKind::Resource,
    )?;
    let attributes = parsed_contents.attributes;
    let ParsedDirectives {
        directives,
        mut depends_on,
    } = parsed_contents.directives.unwrap_or_default();
    depends_on.record_reference_origins(&attributes);
    depends_on.record_binding_reference_origins(
        &attributes,
        &syntax,
        preprocessed_file,
        source_file,
        ctx,
    )?;

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
            Some(RelativeBindingName::new(binding)),
            LoopProvenance::default(),
        ),
        ResourceParseTarget::DeferredForTemplate { target } => (
            target.template_resource_id(
                provider,
                &resource_type,
                directives.provider_instance.clone(),
            ),
            Some(target.template_address().binding_address().clone()),
            LoopProvenance::default(),
        ),
    };
    ctx.remap_binding_use_resource(&provisional_id, &id);

    Ok(Resource {
        id,
        attributes: attributes.into_iter().collect(),
        authored_slots,
        directives,
        resolved_ignore_changes: Default::default(),
        prefixes: HashMap::new(),
        binding,
        dependencies: UnexpandedDependencyFields {
            value_bindings: Default::default(),
            depends_on,
        },
        module_source: None,
        default_tag_keys: Default::default(),
        loop_provenance,
        quoted_string_attrs,
        declaration,
    })
}

pub(crate) fn parse_block_contents_with_mode(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
    mode: &ExpressionMode<'_>,
) -> Result<Option<IndexMap<String, Value>>, ParseError> {
    let contents = parse_block_contents_with_quoted(
        BlockContents::ordinary(pairs),
        ctx,
        mode,
        &mut None,
        ReservedAttributeNamePolicy::Allow,
    )?;
    Ok((!contents.pending).then_some(contents.attributes))
}

struct ParsedBlockContents {
    attributes: IndexMap<String, Value>,
    directives: Option<ParsedDirectives>,
    pending: bool,
    sinks: HashMap<String, crate::parser::retained::SinkExpression>,
}

#[derive(Default)]
struct ParsedDirectives {
    directives: Directives,
    depends_on: crate::parser::UnexpandedDependencies<RelativeBindingName>,
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
    contents: BlockContents<'_>,
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
    let mut sinks = HashMap::new();

    // Local scope extends the parent context with block-scoped let bindings
    let mut local_ctx = ctx.child_scope();
    let mut pending = false;
    let mut pending_names = HashSet::new();

    for content in contents {
        let content_pair = match content {
            BlockContent::Ordinary(pair) => pair,
            BlockContent::Directives(block) => {
                // Keep the first block, including when it is empty. The
                // inventory reports duplicates and inspects every block;
                // later blocks cannot overwrite or evaluate runtime settings.
                if directives.is_none() {
                    check_directives_depends_on_elements(block.clone())?;
                    check_directives_provider_value(block.clone())?;
                    directives = Some(parse_directives(block, &local_ctx)?);
                }
                continue;
            }
        };
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
                        )?
                        .projection()
                        .expect("materialized parser expression")
                        .clone();
                        // Add to local scope only, not to attributes
                        local_ctx.set_variable(name, value);
                    }
                    Rule::attribute => {
                        let mut attr_inner = inner.into_inner();
                        let key_pair =
                            next_pair(&mut attr_inner, "attribute name", "block content")?;
                        let key_span = key_pair.as_span();
                        let key = extract_key_string(key_pair.clone())?;
                        reject_reserved_resource_attribute(
                            &key,
                            key_span,
                            reserved_attribute_names,
                        )?;
                        let value_pair =
                            next_pair(&mut attr_inner, "attribute value", "block content")?;
                        record_quoted_if_literal(quoted_out, &key, &value_pair);
                        let value_mode = mode.at_named_child(key.clone(), &key_pair);
                        // Storage consumes the complete sink and shares its
                        // authored handles with the syntax inventory.
                        let expression =
                            parse_expression_in_sink(value_pair, &local_ctx, &value_mode)?;
                        sinks.insert(key.clone(), expression.clone());
                        match expression.projection() {
                            ParsedSinkValue::Ready(value) => {
                                attributes.insert(key, value.clone());
                            }
                            ParsedSinkValue::Pending(_) => {
                                pending = true;
                                pending_names.insert(key);
                            }
                        }
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name_pair =
                            next_pair(&mut block_inner, "block name", "nested block")?;
                        let block_name = block_name_pair.as_str().to_string();
                        // Recursively parse nested block contents (supports arbitrary depth)
                        let authored_count = authored_nested_blocks
                            .entry(block_name.clone())
                            .or_default();
                        let block_index = *authored_count;
                        *authored_count += 1;
                        let block_mode = mode
                            .at_named_child(block_name.clone(), &block_name_pair)
                            .at_list_index(block_index);
                        let block_contents = parse_block_contents_with_quoted(
                            BlockContents::ordinary(block_inner),
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
                let key = extract_key_string(key_pair.clone())?;
                reject_reserved_resource_attribute(&key, key_span, reserved_attribute_names)?;
                let value_pair = next_pair(&mut attr_inner, "attribute value", "block content")?;
                record_quoted_if_literal(quoted_out, &key, &value_pair);
                let value_mode = mode.at_named_child(key.clone(), &key_pair);
                // This grammar branch stores the same complete sink bundle.
                let expression = parse_expression_in_sink(value_pair, &local_ctx, &value_mode)?;
                sinks.insert(key.clone(), expression.clone());
                match expression.projection() {
                    ParsedSinkValue::Ready(value) => {
                        attributes.insert(key, value.clone());
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
        sinks,
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
    let declaration =
        crate::parser::DeclarationProvenance::from_pair(&pair, preprocessed_file, source_file);
    let syntax = pair.clone();
    let (resource_type, contents) = ResourceBody::from_pair(pair)
        .expect("read parser requires a resource body")
        .into_parts();
    let namespaced_type = resource_type.as_str().to_string();

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
        contents,
        ctx,
        &mode,
        &mut quoted_out,
        ReservedAttributeNamePolicy::Reject,
    )?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();
    let authored_slots = ctx.authored_resource_slots(
        &syntax,
        &parsed_contents.sinks,
        &parsed_contents.attributes,
        crate::schema::SchemaKind::DataSource,
    )?;
    let attributes = parsed_contents.attributes;
    let ParsedDirectives {
        directives,
        depends_on: _,
    } = parsed_contents.directives.unwrap_or_default();

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
        authored_slots,
        directives,
        binding: Some(RelativeBindingName::new(binding_name)),
        dependencies: Default::default(),
        module_source: None,
        quoted_string_attrs,
        declaration,
    })
}

fn parse_directives(
    block: ResourceDirectives<'_>,
    ctx: &ParseContext,
) -> Result<ParsedDirectives, ParseError> {
    let mut parsed_directives = ParsedDirectives::default();

    for entry in block.entries() {
        let attr = match entry {
            DirectiveEntry::Attribute(attribute) => attribute,
            // The inventory retains every malformed entry for validation.
            DirectiveEntry::Invalid(_) => continue,
        };

        let mut attr_inner = attr.into_inner();
        let key = extract_key_string(next_pair(
            &mut attr_inner,
            "attribute name",
            "directives block",
        )?)?;
        let value_pair = next_pair(&mut attr_inner, "attribute value", "directives block")?;
        let line = value_pair.line_col().0;

        let Some(directive) = ResourceDirective::from_name(&key) else {
            // SyntaxInventory retains unknown keys and their locations for
            // collected validation; do not evaluate their unused values.
            continue;
        };

        match directive {
            ResourceDirective::Lifecycle(directive) => {
                let source = ctx.directive_value_source_span(&value_pair);
                let value = parse_expression(value_pair, ctx)?
                    .projection()
                    .expect("materialized parser expression")
                    .clone();
                parsed_directives
                    .directives
                    .set(directive, DirectiveExpr::new(value, source));
            }
            ResourceDirective::IgnoreChanges => {
                let primary =
                    crate::parser::util::unwrap_to_primary(value_pair).ok_or_else(|| {
                        ParseError::InvalidExpression {
                            line,
                            message:
                                "parsed_directives.directives.ignore_changes: must be a list of bare attribute paths"
                                    .to_string(),
                        }
                    })?;
                let list = first_inner(
                    primary,
                    "list",
                    "parsed_directives.directives.ignore_changes",
                )?;
                if list.as_rule() != Rule::list {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message:
                            "parsed_directives.directives.ignore_changes: must be a list of bare attribute paths"
                                .to_string(),
                    });
                }
                parsed_directives.directives.ignore_changes = list
                    .into_inner()
                    .map(|element| parse_ignore_change_path(element, ctx))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            ResourceDirective::DependsOn => {
                let primary =
                    crate::parser::util::unwrap_to_primary(value_pair).ok_or_else(|| {
                        ParseError::InvalidExpression {
                            line,
                            message: "parsed_directives.directives.depends_on: must be a list of binding identifiers"
                                .to_string(),
                        }
                    })?;
                let list = first_inner(primary, "list", "parsed_directives.directives.depends_on")?;
                if list.as_rule() != Rule::list {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: "parsed_directives.directives.depends_on: must be a list of binding identifiers"
                            .to_string(),
                    });
                }
                let mut operands = Vec::new();
                let mut source_lines = Vec::new();
                for element in list.into_inner() {
                    source_lines.push(element.line_col().0);
                    let location = ctx.declaration_location(&element);
                    operands.push(crate::parser::Located::new(
                        location,
                        crate::parser::expressions::dependency::parse_dependency_operand(
                            element,
                            ctx,
                            "parsed_directives.directives.depends_on",
                        )?,
                    ));
                }
                parsed_directives.depends_on =
                    crate::parser::UnexpandedDependencies::from_located(operands);
                parsed_directives.directives.depends_on_diagnostic_sources =
                    DiagnosticSourceLocations::authored(ctx.source_file(), source_lines);
            }
            ResourceDirective::Provider => {
                let eval = parse_expression_eval(value_pair, ctx)?;
                parsed_directives.directives.provider_instance = Some(eval_as_binding_name(
                    eval,
                    "provider: value",
                    line,
                    BindingNameUse::Provider,
                )?);
            }
        }
    }

    Ok(parsed_directives)
}

fn parse_ignore_change_path(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ParsedIgnorePath, ParseError> {
    let line = pair.line_col().0;
    let authored = crate::parser::AuthoredExpressionText::redact(pair.clone(), None);
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
                        match parse_expression(index, ctx)?
                            .projection()
                            .expect("materialized parser expression")
                            .clone()
                        {
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

fn invalid_ignore_change_path(
    line: usize,
    authored: &crate::parser::AuthoredExpressionText,
) -> ParseError {
    ParseError::InvalidAuthoredExpression {
        line,
        diagnostic: crate::parser::AuthoredExpressionDiagnostic::ignore_changes(authored.clone()),
    }
}

#[derive(Clone, Copy)]
enum BindingNameUse {
    Provider,
}

fn eval_as_binding_name(
    eval: EvalValue,
    context: &str,
    line: usize,
    usage: BindingNameUse,
) -> Result<String, ParseError> {
    match eval {
        EvalValue::UnexpandedStructuralHandle(handle) => {
            let accepted = matches!(handle.kind(), UnexpandedStructuralHandleKind::Provider);
            if accepted {
                Ok(handle.canonical_binding_name().to_string())
            } else {
                let binding = handle
                    .alias_name()
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| std::borrow::Cow::Borrowed(handle.canonical_binding_name()));
                let description = handle.kind().description();
                let message = match usage {
                    BindingNameUse::Provider => format!(
                        "directives.provider: '{binding}' is {description}, not a provider instance"
                    ),
                };
                Err(ParseError::InvalidExpression { line, message })
            }
        }
        EvalValue::User(Value::Deferred(DeferredValue::Placeholder(
            crate::resource::ParserPlaceholder::ModuleArgument {
                name,
                diagnostic: _,
            },
        ))) => Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "directives.provider: module parameter '{name}' cannot be used as a provider instance"
            ),
        }),
        EvalValue::User(Value::Deferred(DeferredValue::BindingRef { binding })) => {
            Ok(binding.into_string())
        }
        EvalValue::ProvisionalReference { binding } => Ok(binding.into_string()),
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
fn check_directives_depends_on_elements(block: ResourceDirectives<'_>) -> Result<(), ParseError> {
    for entry in block.entries() {
        let attr = match entry {
            DirectiveEntry::Attribute(attribute) => attribute,
            DirectiveEntry::Invalid(_) => continue,
        };
        let mut attr_inner = attr.into_inner();
        let key_pair = next_pair(&mut attr_inner, "attribute name", "directives attribute")?;
        if ResourceDirective::from_name(&extract_key_string(key_pair)?)
            != Some(ResourceDirective::DependsOn)
        {
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
fn check_directives_provider_value(block: ResourceDirectives<'_>) -> Result<(), ParseError> {
    for entry in block.entries() {
        let attr = match entry {
            DirectiveEntry::Attribute(attribute) => attribute,
            DirectiveEntry::Invalid(_) => continue,
        };
        let mut attr_inner = attr.into_inner();
        let key_pair = next_pair(&mut attr_inner, "attribute name", "directives attribute")?;
        if ResourceDirective::from_name(&extract_key_string(key_pair)?)
            != Some(ResourceDirective::Provider)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ProviderContext, parse_file};
    use std::path::Path;

    #[test]
    fn nested_directives_remain_ordinary_attributes_at_every_depth() {
        for header in [
            "mock.test.resource",
            "let named = mock.test.resource",
            "let observed = read mock.test.resource",
        ] {
            for nested in [
                "directives { value = true }",
                "inner { directives { value = true } }",
                "directives { provider = 'ordinary' depends_on = ['ordinary'] value = true }",
                "directives { value = true } directives { value = false }",
            ] {
                let source = format!("{header} {{ config {{ {nested} }} }}");
                let parsed = parse_file(
                    &source,
                    Path::new("resources.crn"),
                    &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
                )
                .unwrap();
                let attributes = if header.contains("read ") {
                    &parsed.data_sources[0].attributes
                } else {
                    &parsed.resources[0].attributes
                };
                let mut value = &attributes["config"];
                for key in if nested.starts_with("inner") {
                    vec!["inner", "directives", "value"]
                } else {
                    vec!["directives", "value"]
                } {
                    let Value::Concrete(ConcreteValue::List(blocks)) = value else {
                        panic!("expected nested blocks: {source}: {value:?}");
                    };
                    let Value::Concrete(ConcreteValue::Map(fields)) = &blocks[0] else {
                        panic!("expected nested attributes: {source}");
                    };
                    value = fields.get(key).unwrap_or_else(|| {
                        panic!("lost nested attribute {key}: {source}: {fields:?}")
                    });
                }
                assert_eq!(value, &Value::Concrete(ConcreteValue::Bool(true)));
            }
        }
    }

    #[test]
    fn quoted_directive_keys_use_the_ordinary_attribute_identity() {
        for quote in ['\'', '"'] {
            let source = format!(
                "let parent = mock.test.resource {{ name = 'parent' }}\n\
                 mock.test.resource {{\n\
                   name = 'child'\n\
                   directives {{\n\
                     {quote}force_delete{quote} = true\n\
                     {quote}create_before_destroy{quote} = true\n\
                     {quote}prevent_destroy{quote} = true\n\
                     {quote}ignore_changes{quote} = [name]\n\
                     {quote}depends_on{quote} = [parent]\n\
                     {quote}provider{quote} = alternate\n\
                   }}\n\
                 }}\n"
            );
            let parsed = parse_file(
                &source,
                Path::new("resources.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .unwrap();
            assert!(
                crate::validation::resource_directive_findings(
                    parsed.parser_binding_metadata.syntax()
                )
                .is_empty()
            );
            let child = &parsed.resources[1];
            let mut lifecycle_count = 0;
            child.directives.clone().map_expressions(|expression| {
                lifecycle_count += 1;
                assert_eq!(
                    expression.value(),
                    &Value::Concrete(ConcreteValue::Bool(true))
                );
            });
            assert_eq!(lifecycle_count, 3);
            assert_eq!(child.directives.ignore_changes.len(), 1);
            assert!(!child.dependencies.depends_on.is_empty());
            assert_eq!(
                child.directives.provider_instance.as_deref(),
                Some("alternate")
            );
            for kind in [
                crate::parser::elements::ElementKind::Provider,
                crate::parser::elements::ElementKind::IgnoreChanges,
            ] {
                assert!(child.declaration.elements().get(kind).is_some());
            }
        }
    }

    #[test]
    fn duplicate_directives_keep_the_first_block_and_collect_the_second_name() {
        for header in [
            "mock.test.resource",
            "let named = mock.test.resource",
            "let observed = read mock.test.resource",
        ] {
            let source = format!(
                "{header} {{\n  directives {{ prevent_destroy = true }}\n  directives {{ prevent_destroy = false force_delete = absent }}\n}}\n"
            );
            let parsed = parse_file(
                &source,
                Path::new("resources.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .expect("duplicate blocks are collected without evaluating the second block");
            let findings = crate::validation::resource_directive_findings(
                parsed.parser_binding_metadata.syntax(),
            );
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert_eq!(
                findings[0].detail().to_string(),
                "Duplicate attribute 'directives' (first defined on line 2)"
            );
            let directives = if header.contains("read ") {
                &parsed.data_sources[0].directives
            } else {
                &parsed.resources[0].directives
            };
            let mut values = Vec::new();
            directives
                .clone()
                .map_expressions(|expression| values.push(expression.value().clone()));
            assert_eq!(values, [Value::Concrete(ConcreteValue::Bool(true))]);
            let location = findings[0].location();
            assert_eq!(location.file().path(), Path::new("resources.crn"));
            assert_eq!(
                (location.span().start_line(), location.span().start_column()),
                (3, 3)
            );
            assert_eq!(
                (location.span().end_line(), location.span().end_column()),
                (3, 13)
            );
        }
    }

    #[test]
    fn duplicate_directives_in_unevaluated_resources_are_rejected() {
        for wrapper in ["let empty = for n in []", "let hidden = if false"] {
            let source = format!(
                "{wrapper} {{\n  mock.test.resource {{\n    directives {{}}\n    directives {{}}\n  }}\n}}\n"
            );
            let parsed = parse_file(
                &source,
                Path::new("resources.crn"),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .expect("duplicate blocks must not abort the inventory");
            let findings = crate::validation::resource_directive_findings(
                parsed.parser_binding_metadata.syntax(),
            );
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert_eq!(
                findings[0].detail().to_string(),
                "Duplicate attribute 'directives' (first defined on line 3)"
            );
            let location = findings[0].location();
            assert_eq!(
                (location.span().start_line(), location.span().start_column()),
                (4, 5)
            );
            assert_eq!(
                (location.span().end_line(), location.span().end_column()),
                (4, 15)
            );
        }
    }
}
