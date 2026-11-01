//! Type-expression parser (`String`, `List(...)`, `Map(...)`, `Ref`,
//! `SchemaType`, `Struct`).
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use crate::schema::{TypeIdentity, levenshtein_distance};
use crate::static_type::StaticTypeResolutionError;

use super::ast::{ResourceTypePath, TypeExpr};
use super::error::{ParseError, ParseWarning};
use super::expressions::string_literal::parse_string_literal;
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
    let mut pairs = super::grammar::parse_type_expr_fragment(trimmed).ok()?;
    let pair = pairs.next()?;
    if pair.as_span().end() != trimmed.len() {
        return None;
    }
    let mut warnings: Vec<ParseWarning> = Vec::new();
    parse_type_expr(pair, config, &mut warnings, None).ok()
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
    preprocessed_file: Option<&super::PreprocessedFile>,
) -> Result<TypeExpr, ParseError> {
    // Top-level `type_expr` is one or more `type_expr_atom`s separated
    // by `|`. Collect each atom; the `|` tokens are silent in pest, so
    // pair iteration yields only `type_expr_atom` children.
    let mut atoms: Vec<TypeExpr> = Vec::new();
    for child in pair.into_inner() {
        if child.as_rule() == Rule::type_expr_atom {
            atoms.push(parse_type_expr_atom(
                child,
                config,
                warnings,
                preprocessed_file,
            )?);
        }
    }
    match atoms.len() {
        0 => Ok(TypeExpr::String),
        1 => Ok(atoms.into_iter().next().unwrap()),
        _ => Ok(TypeExpr::Union(atoms)),
    }
}

/// Constructor-backed bare custom types built into the DSL, keyed by
/// snake-case name and sorted for deterministic tooling output. Keeping each
/// declaration preserves its scalar kind for static type lifting.
///
/// Derived from [`crate::schema::builtin_type_registry`], the same source used
/// for provider admission and validator reconstruction.
pub fn builtin_bare_custom_types()
-> &'static std::collections::BTreeMap<String, crate::schema::AttributeType> {
    static TYPES: std::sync::OnceLock<
        std::collections::BTreeMap<String, crate::schema::AttributeType>,
    > = std::sync::OnceLock::new();
    TYPES.get_or_init(|| {
        crate::schema::builtin_type_registry()
            .iter()
            .map(|(identity, declaration)| (pascal_to_snake(&identity.kind), declaration.clone()))
            .collect()
    })
}

/// Resolve a snake-cased bare custom-type annotation without discarding its
/// schema owner.
///
/// Core built-ins remain [`TypeExpr::Simple`]. Every schema-declared bare
/// identity becomes [`TypeExpr::SchemaType`], including identities whose
/// semantic provider axis is empty. This is the owner-bearing counterpart to
/// dotted type resolution and prevents a bare provider type from falling back
/// to an identity-only validation route.
pub(crate) fn resolve_known_bare_custom_type(
    snake: &str,
    config: &ProviderContext,
) -> Option<TypeExpr> {
    if builtin_bare_custom_types().contains_key(snake) {
        return Some(TypeExpr::Simple(snake.to_string()));
    }
    let pascal = snake_to_pascal(snake);
    let matches = config
        .known_custom_types()
        .iter()
        .filter(|owned| {
            owned.identity().provider.is_none()
                && owned.identity().segments.is_empty()
                && owned.identity().kind == pascal
        })
        .collect::<Vec<_>>();
    let first = matches.first()?;
    let owners = matches
        .iter()
        .skip(1)
        .fold(first.owners().clone(), |owners, owned| {
            owners.union(owned.owners())
        });
    let owned = crate::parser::OwnedTypeIdentity::with_owners(first.identity().clone(), owners);
    Some(TypeExpr::SchemaType { owned })
}

fn custom_type_suggestion_key(id: &TypeIdentity) -> String {
    id.segments
        .iter()
        .chain(std::iter::once(&id.kind))
        .map(|part| pascal_to_snake(part))
        .collect::<Vec<_>>()
        .join("_")
}

fn custom_type_input_key(input: &str) -> String {
    if input.contains('.') {
        let id = TypeIdentity::from_dotted(input);
        custom_type_suggestion_key(&id)
    } else {
        pascal_to_snake(input)
    }
}

pub(crate) fn suggest_registered_dotted_custom_type(
    input: &str,
    config: &ProviderContext,
) -> Option<String> {
    let input_key = custom_type_input_key(input);
    let mut candidates: Vec<(&super::OwnedTypeIdentity, String)> = config
        .known_custom_types()
        .iter()
        .filter(|owned| owned.provider_names().next().is_some())
        .map(|owned| (owned, custom_type_suggestion_key(owned.identity())))
        .collect();
    candidates.sort_by_key(|(owned, _)| owned.authored_name());

    candidates
        .iter()
        .find(|(_, key)| key == &input_key)
        .map(|(owned, _)| owned.authored_name())
        .or_else(|| {
            let max_distance = match input_key.len() {
                0..=2 => 1,
                3..=5 => 2,
                _ => 3,
            };
            candidates
                .iter()
                .map(|(owned, key)| (*owned, levenshtein_distance(&input_key, key)))
                .filter(|(_, distance)| *distance <= max_distance)
                .min_by_key(|(_, distance)| *distance)
                .map(|(owned, _)| owned.authored_name())
        })
}

pub(crate) fn unknown_custom_type_error(
    input: &str,
    config: &ProviderContext,
) -> StaticTypeResolutionError {
    match suggest_registered_dotted_custom_type(input, config) {
        Some(suggestion) => StaticTypeResolutionError::UnknownCustomTypeWithSuggestion {
            name: input.to_string(),
            suggestion,
        },
        None => StaticTypeResolutionError::UnknownCustomType {
            name: input.to_string(),
        },
    }
}

pub(crate) fn unknown_custom_type_message(input: &str, config: &ProviderContext) -> String {
    unknown_custom_type_error(input, config).to_string()
}

fn parse_type_expr_atom(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
    preprocessed_file: Option<&super::PreprocessedFile>,
) -> Result<TypeExpr, ParseError> {
    let _ = warnings;
    let inner = first_inner(pair, "type", "type expression")?;
    match inner.as_rule() {
        Rule::type_string_literal => {
            let string = first_inner(inner, "string", "string literal type")?;
            Ok(TypeExpr::StringLiteral(parse_string_literal(string)?))
        }
        Rule::type_simple => {
            let type_span = preprocessed_file.map_or_else(
                || super::SourceSpan::from_unmapped_pest(&inner),
                |file| super::SourceSpan::from_pest(&inner, file),
            );
            let line = type_span.start_line();
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
                    if config.custom_types_loaded() {
                        return match resolve_known_bare_custom_type(&snake, config) {
                            Some(resolved) => Ok(resolved),
                            None => Err(ParseError::unresolved_declaration_type(
                                other,
                                type_span,
                                unknown_custom_type_message(other, config),
                            )),
                        };
                    }
                    Ok(TypeExpr::Simple(snake))
                }
                other => Err(ParseError::InvalidExpression {
                    line,
                    message: match suggest_registered_dotted_custom_type(other, config) {
                        Some(suggestion) => format!(
                            "unknown type '{other}'; custom types must use a registered type name; suggestion: use '{suggestion}'"
                        ),
                        None => format!(
                            "unknown type '{other}'; custom types must use a registered type name"
                        ),
                    },
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
                preprocessed_file,
            )?;

            if is_list {
                Ok(TypeExpr::List(Box::new(inner_type)))
            } else {
                Ok(TypeExpr::Map(Box::new(inner_type)))
            }
        }
        Rule::type_ref => {
            // Dotted type paths are deliberately left unresolved during parsing:
            // bootstrap contexts do not yet know provider schemas or custom
            // validators. Enriched validation resolves this to either Ref or
            // SchemaType and rejects unknown dotted paths.
            let mut ref_inner = inner.into_inner();
            let path_str = next_pair(&mut ref_inner, "resource type path", "type ref")?.as_str();
            let path = ResourceTypePath::parse(path_str).ok_or_else(|| {
                ParseError::InvalidResourceType(format!("Invalid resource type path: {}", path_str))
            })?;
            Ok(TypeExpr::DottedUnresolved(path))
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
                        preprocessed_file,
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

    fn loaded_context(
        known: std::collections::HashSet<crate::parser::OwnedTypeIdentity>,
    ) -> ProviderContext {
        ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                known,
                Box::new(|target, _value| {
                    Err(
                        crate::parser::CustomTypeValidationError::MissingDeclaration {
                            owner: "test".to_string(),
                            identity: target.identity().clone(),
                        },
                    )
                }),
            ),
            ..ProviderContext::default()
        }
    }

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
    fn escaped_string_literal_round_trips_through_display() {
        let ctx = ProviderContext::default();
        let parsed = parse_type_expr_str(r"'a\\b'", &ctx).expect("type literal should parse");

        assert_eq!(parsed, TypeExpr::StringLiteral(r"a\b".to_string()));
        assert_eq!(parse_type_expr_str(&parsed.to_string(), &ctx), Some(parsed));
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

    /// Default `ProviderContext` (no provider phase has run yet, so custom
    /// types are `Unloaded`) keeps the legacy lax behavior: any
    /// PascalCase identifier becomes `TypeExpr::Simple(snake_case)`. This
    /// preserves the LSP early-parse path that runs before schemas are
    /// loaded — without it, every mid-edit buffer would lose all
    /// `Simple`-shaped type completions.
    #[test]
    fn parse_type_expr_str_accepts_unknown_when_custom_types_are_unloaded() {
        let ctx = ProviderContext::default();
        assert!(matches!(
            parse_type_expr_str("TotallyMadeUpType", &ctx),
            Some(TypeExpr::Simple(ref s)) if s == "totally_made_up_type"
        ));
    }

    /// After the provider phase has produced `ProviderCustomTypes::Loaded`, an
    /// unknown bare PascalCase custom-type name in a type position is
    /// rejected at parse time rather than silently accepted as
    /// `TypeExpr::Simple`. This is the carina#3239 fix: typos and
    /// renamed-then-removed types stop being silent.
    #[test]
    fn parse_type_expr_str_rejects_unknown_when_custom_types_are_loaded() {
        let ctx = loaded_context(Default::default());
        // No provider validators registered → only core built-ins are valid.
        // A clearly-fake name is rejected.
        assert_eq!(parse_type_expr_str("TotallyMadeUpType", &ctx), None);
        // A historical-but-removed name is rejected the same way — the
        // bug-headline example from carina#3239.
        assert_eq!(parse_type_expr_str("IamOidcProviderArn", &ctx), None);
    }

    #[test]
    fn parse_type_expr_str_accepts_every_builtin_validator_when_custom_types_are_loaded() {
        let ctx = loaded_context(Default::default());
        for identity in crate::schema::builtin_type_registry().keys() {
            assert!(identity.provider.is_none() && identity.segments.is_empty());
            let name = &identity.kind;
            assert_eq!(
                parse_type_expr_str(name, &ctx),
                Some(TypeExpr::Simple(pascal_to_snake(name))),
                "registered built-in {name} must be accepted without a provider"
            );
        }
    }

    /// A provider-owned bare custom-type identity registered in
    /// `ProviderContext.known_custom_types`
    /// makes the corresponding PascalCase name acceptable in a type
    /// position. Dotted identities (`aws.iam.Role.Arn`) are parsed as
    /// unresolved paths, not here — `TypeExpr::Simple` only carries the
    /// *bare* axis.
    #[test]
    fn parse_type_expr_str_accepts_registered_bare_custom_when_custom_types_are_loaded() {
        use crate::schema::TypeIdentity;

        let ctx = loaded_context(std::collections::HashSet::from([
            crate::parser::OwnedTypeIdentity::provider("test", TypeIdentity::bare("EmailAddress")),
        ]));
        assert!(matches!(
            parse_type_expr_str("EmailAddress", &ctx),
            Some(TypeExpr::SchemaType { owned })
                if owned.identity() == &TypeIdentity::bare("EmailAddress")
                    && owned.provider_names().eq(["test"])
        ));
        // A bare key does NOT make an unrelated name valid.
        assert_eq!(parse_type_expr_str("TotallyMadeUpType", &ctx), None);
    }
}
