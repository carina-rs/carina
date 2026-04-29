//! Resource-expression parsers (`provider.service.Type "name" { ... }`,
//! anonymous resources, `read` data sources) and the shared
//! `parse_block_contents` traversal that backs both resources and the
//! map-literal primary.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::parser::Rule;
use crate::parser::blocks::attributes::extract_lifecycle_config;
use crate::parser::context::{ParseContext, extract_key_string, first_inner, next_pair};
use crate::parser::error::ParseError;
use crate::parser::parse_expression;
use crate::parser::util::expression_is_plain_string_literal;
use crate::resource::{Expr, Resource, ResourceId, ResourceKind, Value};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};

pub(in crate::parser) fn parse_anonymous_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Resource, ParseError> {
    let inner = pair.into_inner();

    let mut iter = inner;
    let namespaced_type = next_pair(&mut iter, "resource type", "anonymous resource")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let mut quoted_out: Option<HashSet<String>> = Some(HashSet::new());
    let attributes = parse_block_contents_with_quoted(iter, ctx, &mut quoted_out)?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();

    // Anonymous resources get an empty name that will be replaced by a hash-based
    // identifier computed from create-only properties after parsing.
    let resource_name = String::new();

    let mut attributes = attributes;
    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    let id = ResourceId::with_provider(provider, resource_type, resource_name);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::Real,
        lifecycle,
        prefixes: HashMap::new(),
        binding: None,
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        quoted_string_attrs,
    })
}

/// Parse block contents (attributes, nested blocks, and local let bindings)
/// Nested blocks with the same name are collected into a list.
/// Local let bindings are resolved within the block scope and NOT included in
/// the returned attributes.
pub(crate) fn parse_block_contents(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
) -> Result<IndexMap<String, Value>, ParseError> {
    parse_block_contents_with_quoted(pairs, ctx, &mut None)
}

/// As [`parse_block_contents`], but if `quoted_out` is `Some`, populate it
/// with the names of top-level attributes whose value is a plain quoted
/// string literal (`attr = "..."`). Used by resource-level callers to
/// build `Resource.quoted_string_attrs` for enum-attribute diagnostics
/// (#2094 / #2229) without re-walking the pest tree.
pub(in crate::parser) fn parse_block_contents_with_quoted(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
    quoted_out: &mut Option<HashSet<String>>,
) -> Result<IndexMap<String, Value>, ParseError> {
    // `IndexMap` so the order in which the user wrote attributes in the
    // .crn file flows all the way to `Resource.attributes` and to
    // `Value::Map` payloads — anything that re-renders attributes
    // (formatter, plan display, diagnostics) sees a stable order.
    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();

    // Local scope extends the parent context with block-scoped let bindings
    let mut local_ctx = ctx.clone();

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
                        let key = extract_key_string(key_pair)?;
                        let value_pair =
                            next_pair(&mut attr_inner, "attribute value", "block content")?;
                        record_quoted_if_literal(quoted_out, &key, &value_pair);
                        let value = parse_expression(value_pair, &local_ctx)?;
                        attributes.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name = next_pair(&mut block_inner, "block name", "nested block")?
                            .as_str()
                            .to_string();

                        // Recursively parse nested block contents (supports arbitrary depth)
                        let block_attrs = parse_block_contents(block_inner, &local_ctx)?;

                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            Rule::attribute => {
                let mut attr_inner = content_pair.into_inner();
                let key_pair = next_pair(&mut attr_inner, "attribute name", "block content")?;
                let key = extract_key_string(key_pair)?;
                let value_pair = next_pair(&mut attr_inner, "attribute value", "block content")?;
                record_quoted_if_literal(quoted_out, &key, &value_pair);
                let value = parse_expression(value_pair, &local_ctx)?;
                attributes.insert(key, value);
            }
            _ => {}
        }
    }

    // Convert nested blocks to list attributes
    for (name, blocks) in nested_blocks {
        attributes.insert(name, Value::List(blocks));
    }

    Ok(attributes)
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
) -> Result<Resource, ParseError> {
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "resource expression")?
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

    let mut quoted_out: Option<HashSet<String>> = Some(HashSet::new());
    let mut attributes = parse_block_contents_with_quoted(inner, ctx, &mut quoted_out)?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));

    let id = ResourceId::with_provider(provider, resource_type, resource_name);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::Real,
        lifecycle,
        prefixes: HashMap::new(),
        binding: Some(binding_name.to_string()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        quoted_string_attrs,
    })
}

/// Parse a read resource expression (data source): read aws.s3_bucket { ... }
pub(crate) fn parse_read_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
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

    let mut quoted_out: Option<HashSet<String>> = Some(HashSet::new());
    let mut attributes = parse_block_contents_with_quoted(inner, ctx, &mut quoted_out)?;
    let quoted_string_attrs = quoted_out.unwrap_or_default();

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));
    // Mark as data source
    attributes.insert("_data_source".to_string(), Value::Bool(true));

    let id = ResourceId::with_provider(provider, resource_type, resource_name);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::DataSource,
        lifecycle,
        prefixes: HashMap::new(),
        binding: Some(binding_name.to_string()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
        quoted_string_attrs,
    })
}
