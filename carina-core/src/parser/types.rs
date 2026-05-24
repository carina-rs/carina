//! Type-expression parser (`String`, `List(...)`, `Map(...)`, `Ref`,
//! `SchemaType`, `Struct`).
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use pest::Parser;

use super::ast::{ResourceTypePath, TypeExpr};
use super::error::{ParseError, ParseWarning};
use super::util::{pascal_to_snake, snake_to_pascal};
use super::{ProviderContext, Rule, first_inner, next_pair};

/// Parse a standalone type-expression text snippet (e.g. `"String"`,
/// `"List(Int)"`, `"'dev' | 'prod'"`) into a [`TypeExpr`].
///
/// Returns `None` when the input doesn't match the type-expression
/// grammar — callers can use this to short-circuit type-aware
/// behavior (e.g. LSP completion ranking) without aborting; an
/// unparseable type hint just falls back to the un-typed path.
///
/// The full trimmed input must match — trailing tokens cause `None`
/// rather than a silent prefix match. This matters for mid-edit
/// buffers like `param: Bool foo` where pest's default greedy
/// behavior would otherwise accept `Bool` and drop ` foo`, producing
/// a wrong type that would then drive completion filtering.
///
/// This wraps the internal pest entry point so consumers outside
/// the parser module (LSP, tests, tooling) can lift textual type
/// hints into the type system without re-implementing pest plumbing.
pub fn parse_type_expr_str(input: &str, config: &ProviderContext) -> Option<TypeExpr> {
    let trimmed = input.trim();
    let mut pairs = super::CarinaParser::parse(super::Rule::type_expr, trimmed).ok()?;
    let pair = pairs.next()?;
    if pair.as_span().end() != trimmed.len() {
        return None;
    }
    let mut warnings: Vec<ParseWarning> = Vec::new();
    parse_type_expr(pair, config, &mut warnings).ok()
}

/// Parse type expression. Handles unions of atomic types
/// (`'dev' | 'prod'`, see carina-rs/carina#2611) by collecting every
/// `type_expr_atom` child and folding 2+ atoms into a [`TypeExpr::Union`];
/// a single atom returns the atom unchanged so existing call sites
/// keep their shape.
pub(super) fn parse_type_expr(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<TypeExpr, ParseError> {
    // Top-level `type_expr` is one or more `type_expr_atom`s separated
    // by `|`. Collect each atom; the `|` tokens are silent in pest, so
    // pair iteration yields only `type_expr_atom` children.
    let mut atoms: Vec<TypeExpr> = Vec::new();
    for child in pair.into_inner() {
        if child.as_rule() == Rule::type_expr_atom {
            atoms.push(parse_type_expr_atom(child, config, warnings)?);
        }
    }
    match atoms.len() {
        0 => Ok(TypeExpr::String),
        1 => Ok(atoms.into_iter().next().unwrap()),
        _ => Ok(TypeExpr::Union(atoms)),
    }
}

/// The set of bare PascalCase custom types built into the DSL itself.
/// These are accepted in a type position regardless of which providers
/// (if any) have registered, because their validators live in
/// `carina-core` rather than in any provider — see the matching arms in
/// [`crate::parser::functions::validate_custom_type`].
const BUILTIN_BARE_CUSTOM_TYPES: &[&str] =
    &["ipv4_cidr", "ipv4_address", "ipv6_cidr", "ipv6_address"];

/// True iff a snake-cased bare type name resolves to a known custom
/// type — either a `carina-core` built-in or an identity registered in
/// `config.validators` with no provider/segment axis. The carina#3239
/// strict-parse gate consults this when
/// [`ProviderContext::customs_loaded`] is `true`.
///
/// `TypeExpr::Simple` only carries the bare axis, so this check is
/// deliberately scoped to bare identities — provider-scoped types like
/// `aws.iam.Role.Arn` arrive through the `TypeExpr::SchemaType` path
/// instead and are validated by `schema_types`.
///
/// Exposed at the crate root so the validation layer
/// (`crate::validation`) can apply the same predicate to argument
/// declarations parsed earlier with a bootstrap context — the
/// standalone-module-validate path where the strict parse-time gate
/// did not fire.
pub(crate) fn is_known_bare_custom_type(snake: &str, config: &ProviderContext) -> bool {
    if BUILTIN_BARE_CUSTOM_TYPES.contains(&snake) {
        return true;
    }
    let pascal = snake_to_pascal(snake);
    config
        .validators
        .keys()
        .any(|id| id.provider.is_none() && id.segments.is_empty() && id.kind == pascal)
}

fn parse_type_expr_atom(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<TypeExpr, ParseError> {
    let _ = warnings;
    let inner = first_inner(pair, "type", "type expression")?;
    match inner.as_rule() {
        Rule::type_string_literal => {
            // type_string_literal wraps a `string` rule. Unquote and
            // strip interpolation: type-position string literals are
            // by construction simple literals (the grammar accepts
            // only the same `string` form, but interpolation in a
            // type position has no semantics and is rejected as
            // "unknown type" upstream rather than crashed on here).
            let raw = inner.as_str();
            let unquoted = if (raw.starts_with('\'') && raw.ends_with('\''))
                || (raw.starts_with('"') && raw.ends_with('"'))
            {
                &raw[1..raw.len() - 1]
            } else {
                raw
            };
            Ok(TypeExpr::StringLiteral(unquoted.to_string()))
        }
        Rule::type_simple => {
            let line = inner.as_span().start_pos().line_col().0;
            let text = inner.as_str();
            match text {
                "String" => Ok(TypeExpr::String),
                "Bool" => Ok(TypeExpr::Bool),
                "Int" => Ok(TypeExpr::Int),
                "Float" => Ok(TypeExpr::Float),
                "Duration" => Ok(TypeExpr::Duration),
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
                    let snake = pascal_to_snake(other);
                    // carina#3239: when the provider-registration phase
                    // has populated `config`, an unknown bare custom-type
                    // name in a type position is a parse error. Before
                    // this gate, any PascalCase identifier became
                    // `TypeExpr::Simple(snake)` and downstream `validate`
                    // silently treated unknowns as untyped strings — so
                    // typos and renamed-then-removed types went unnoticed.
                    if config.customs_loaded && !is_known_bare_custom_type(&snake, config) {
                        return Err(ParseError::InvalidExpression {
                            line,
                            message: format!("unknown custom type '{other}'"),
                        });
                    }
                    Ok(TypeExpr::Simple(snake))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_type_expr_str_accepts_primitive() {
        let ctx = ProviderContext::default();
        assert!(matches!(
            parse_type_expr_str("Bool", &ctx),
            Some(TypeExpr::Bool)
        ));
        assert!(matches!(
            parse_type_expr_str("String", &ctx),
            Some(TypeExpr::String)
        ));
        assert!(matches!(
            parse_type_expr_str("Duration", &ctx),
            Some(TypeExpr::Duration)
        ));
    }

    #[test]
    fn parse_type_expr_str_trims_surrounding_whitespace() {
        let ctx = ProviderContext::default();
        assert!(matches!(
            parse_type_expr_str("  String  ", &ctx),
            Some(TypeExpr::String)
        ));
    }

    #[test]
    fn parse_type_expr_str_rejects_trailing_junk() {
        // Without the end-of-input check this would silently match
        // `Bool` and drop ` foo`, producing a wrong type. Mid-edit
        // buffers where the user has typed past the type need the
        // bypass — `None` here lets the completion filter fall
        // through to "show everything".
        let ctx = ProviderContext::default();
        assert_eq!(parse_type_expr_str("Bool foo", &ctx), None);
    }

    #[test]
    fn parse_type_expr_str_pascal_simple() {
        let ctx = ProviderContext::default();
        // PascalCase identifier → TypeExpr::Simple(snake_case)
        assert!(matches!(
            parse_type_expr_str("IamOidcProviderArn", &ctx),
            Some(TypeExpr::Simple(ref s)) if s == "iam_oidc_provider_arn"
        ));
    }

    #[test]
    fn parse_type_expr_str_rejects_unparseable() {
        let ctx = ProviderContext::default();
        assert_eq!(parse_type_expr_str("!!!", &ctx), None);
        assert_eq!(parse_type_expr_str("List(", &ctx), None);
        assert_eq!(parse_type_expr_str("", &ctx), None);
    }

    /// Default `ProviderContext` (no provider phase has run yet, so
    /// `customs_loaded` is false) keeps the legacy lax behavior: any
    /// PascalCase identifier becomes `TypeExpr::Simple(snake_case)`. This
    /// preserves the LSP early-parse path that runs before schemas are
    /// loaded — without it, every mid-edit buffer would lose all
    /// `Simple`-shaped type completions.
    #[test]
    fn parse_type_expr_str_accepts_unknown_when_customs_not_loaded() {
        let ctx = ProviderContext::default();
        assert!(matches!(
            parse_type_expr_str("TotallyMadeUpType", &ctx),
            Some(TypeExpr::Simple(ref s)) if s == "totally_made_up_type"
        ));
    }

    /// After the provider phase has run (`customs_loaded = true`), an
    /// unknown bare PascalCase custom-type name in a type position is
    /// rejected at parse time rather than silently accepted as
    /// `TypeExpr::Simple`. This is the carina#3239 fix: typos and
    /// renamed-then-removed types stop being silent.
    #[test]
    fn parse_type_expr_str_rejects_unknown_when_customs_loaded() {
        let ctx = ProviderContext {
            customs_loaded: true,
            ..Default::default()
        };
        // No validators registered → only the four built-ins are valid.
        // A clearly-fake name is rejected.
        assert_eq!(parse_type_expr_str("TotallyMadeUpType", &ctx), None);
        // A historical-but-removed name is rejected the same way — the
        // bug-headline example from carina#3239.
        assert_eq!(parse_type_expr_str("IamOidcProviderArn", &ctx), None);
    }

    /// Built-in DSL custom types (`Ipv4Cidr`, `Ipv4Address`, `Ipv6Cidr`,
    /// `Ipv6Address`) are always accepted, even when no provider has
    /// registered any validators — they are part of `carina-core` itself.
    #[test]
    fn parse_type_expr_str_accepts_builtins_when_customs_loaded() {
        let ctx = ProviderContext {
            customs_loaded: true,
            ..Default::default()
        };
        for name in ["Ipv4Cidr", "Ipv4Address", "Ipv6Cidr", "Ipv6Address"] {
            assert!(
                matches!(parse_type_expr_str(name, &ctx), Some(TypeExpr::Simple(_))),
                "built-in custom type '{name}' must parse with customs_loaded=true"
            );
        }
    }

    /// A bare custom-type identity registered in `ProviderContext.validators`
    /// makes the corresponding PascalCase name acceptable in a type
    /// position. Provider-scoped identities (`aws.iam.Role.Arn`) live on
    /// the `TypeExpr::SchemaType` path, not here — `TypeExpr::Simple`
    /// only carries the *bare* axis.
    #[test]
    fn parse_type_expr_str_accepts_registered_bare_custom_when_customs_loaded() {
        use crate::schema::TypeIdentity;

        let mut ctx = ProviderContext {
            customs_loaded: true,
            ..Default::default()
        };
        ctx.validators
            .insert(TypeIdentity::bare("EmailAddress"), Box::new(|_| Ok(())));
        assert!(matches!(
            parse_type_expr_str("EmailAddress", &ctx),
            Some(TypeExpr::Simple(ref s)) if s == "email_address"
        ));
        // A bare key does NOT make an unrelated name valid.
        assert_eq!(parse_type_expr_str("TotallyMadeUpType", &ctx), None);
    }
}
