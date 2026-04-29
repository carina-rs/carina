//! Type-expression parser (`String`, `List(...)`, `Map(...)`, `Ref`,
//! `SchemaType`, `Struct`).
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::ast::{ResourceTypePath, TypeExpr};
use super::error::{ParseError, ParseWarning};
use super::util::{pascal_to_snake, snake_to_pascal};
use super::{ProviderContext, Rule, first_inner, next_pair};

/// Parse type expression
pub(super) fn parse_type_expr(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<TypeExpr, ParseError> {
    let _ = warnings;
    let inner = first_inner(pair, "type", "type expression")?;
    match inner.as_rule() {
        Rule::type_simple => {
            let line = inner.as_span().start_pos().line_col().0;
            let text = inner.as_str();
            match text {
                "String" => Ok(TypeExpr::String),
                "Bool" => Ok(TypeExpr::Bool),
                "Int" => Ok(TypeExpr::Int),
                "Float" => Ok(TypeExpr::Float),
                // Phase C: the transition window for snake_case primitives
                // and custom types has closed. The parser accepts only
                // PascalCase type names (naming-conventions design D1).
                "string" | "bool" | "int" | "float" => Err(ParseError::InvalidExpression {
                    line,
                    message: format!(
                        "unknown type '{text}'; primitive types are PascalCase — use '{}' instead",
                        snake_to_pascal(text)
                    ),
                }),
                other if other.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
                    Ok(TypeExpr::Simple(pascal_to_snake(other)))
                }
                other => Err(ParseError::InvalidExpression {
                    line,
                    message: format!(
                        "unknown type '{other}'; custom types are PascalCase — use '{}' instead",
                        snake_to_pascal(other)
                    ),
                }),
            }
        }
        Rule::type_generic => {
            // Get the full string representation to determine if it's list or map
            let full_str = inner.as_str();
            let is_list = full_str.starts_with("list");

            // Get the inner type expression
            let mut generic_inner = inner.into_inner();
            let inner_type = parse_type_expr(
                next_pair(&mut generic_inner, "inner type", "generic type expression")?,
                config,
                warnings,
            )?;

            if is_list {
                Ok(TypeExpr::List(Box::new(inner_type)))
            } else {
                Ok(TypeExpr::Map(Box::new(inner_type)))
            }
        }
        Rule::type_ref => {
            // Parse resource_type_path directly (e.g., aws.vpc or awscc.ec2.VpcId)
            let mut ref_inner = inner.into_inner();
            let path_str = next_pair(&mut ref_inner, "resource type path", "type ref")?.as_str();
            let parts: Vec<&str> = path_str.split('.').collect();

            // A 3+ segment path with a PascalCase final segment is ambiguous:
            // `aws.ec2.Vpc` is a resource kind (Ref), `awscc.ec2.VpcId` is a
            // schema type. Disambiguate by asking the provider context:
            // registered schema types become SchemaType, everything else
            // falls back to Ref.
            let has_pascal_tail = parts.len() >= 3
                && parts
                    .last()
                    .is_some_and(|s| s.starts_with(|c: char| c.is_uppercase()));
            if has_pascal_tail {
                let provider = parts[0];
                let path = parts[1..parts.len() - 1].join(".");
                let type_name = parts.last().unwrap();
                if config.is_schema_type(provider, &path, type_name) {
                    return Ok(TypeExpr::SchemaType {
                        provider: provider.to_string(),
                        path,
                        type_name: type_name.to_string(),
                    });
                }
            }
            let path = ResourceTypePath::parse(path_str).ok_or_else(|| {
                ParseError::InvalidResourceType(format!("Invalid resource type path: {}", path_str))
            })?;
            Ok(TypeExpr::Ref(path))
        }
        Rule::type_struct => {
            let mut fields: Vec<(String, TypeExpr)> = Vec::new();
            for child in inner.into_inner() {
                if child.as_rule() != Rule::struct_field_list {
                    continue;
                }
                for field_pair in child.into_inner() {
                    if field_pair.as_rule() != Rule::struct_field {
                        continue;
                    }
                    let mut field_inner = field_pair.into_inner();
                    let name = next_pair(&mut field_inner, "field name", "struct field")?
                        .as_str()
                        .to_string();
                    let ty = parse_type_expr(
                        next_pair(&mut field_inner, "field type", "struct field")?,
                        config,
                        warnings,
                    )?;
                    if fields.iter().any(|(existing, _)| existing == &name) {
                        return Err(ParseError::InvalidResourceType(format!(
                            "struct has duplicate field name '{name}'"
                        )));
                    }
                    fields.push((name, ty));
                }
            }
            Ok(TypeExpr::Struct { fields })
        }
        _ => Ok(TypeExpr::String),
    }
}
