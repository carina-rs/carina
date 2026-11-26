//! Unavailable inspection uses kernel-issued protection and expression edges.
//! All policies re-enter Secret; saved verification selects which failures
//! count, while resolution excludes only a not-yet-selected coalesce default.

use super::*;
use crate::{config_loader::ResolutionContext, parser::ParseError, resource::InsideSecret};

pub(crate) fn find_required_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    context: ResolutionContext,
) -> Result<Option<ParseError>, WalkError> {
    if !context.requires_available() {
        return Ok(None);
    }
    find_unavailable(value, UnavailableSearchPolicy::Required(context))
}

#[derive(Clone, Copy)]
pub(crate) enum UnavailableSearchPolicy {
    Required(ResolutionContext),
    /// Every candidate operand, including Secret and coalesce defaults.
    SavedCandidate,
    /// Only failures with a Secret ancestor, including coalesce defaults.
    SavedSecret,
}

impl UnavailableSearchPolicy {
    fn includes(self, at: &At<'_>) -> bool {
        // The former search inspected call arguments, not captured function
        // definitions/scopes or auxiliary access provenance. Keep that exact
        // expression surface while the kernel still inventories every edge.
        if at.expression_edges().iter().any(|edge| {
            matches!(
                edge,
                ExpressionEdge::FunctionDefinition | ExpressionEdge::FunctionScope(_)
            ) || matches!(edge, ExpressionEdge::ArgumentValue(index) if *index != 0)
        }) {
            return false;
        }
        match self {
            Self::Required(context) => context
                .at_expression_edges(at.expression_edges())
                .requires_available(),
            Self::SavedCandidate => true,
            Self::SavedSecret => at.is_secret(),
        }
    }
}

pub(crate) fn contains_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    policy: UnavailableSearchPolicy,
) -> Result<bool, WalkError> {
    find_unavailable(value, policy).map(|error| error.is_some())
}

fn find_unavailable<P: ValueKind, B: BindingAddress>(
    value: &Value<P, B>,
    policy: UnavailableSearchPolicy,
) -> Result<Option<ParseError>, WalkError> {
    let mut search = UnavailableSearch {
        policy,
        errors: Vec::new(),
    };
    let result =
        inspect(WalkRoot::syntactic(), value, &mut search).map(|located| located.into_output());
    finish_unavailable_search(result, search.errors)
}

fn finish_unavailable_search(
    result: Result<Evidence<usize, ()>, WalkError>,
    errors: Vec<ParseError>,
) -> Result<Option<ParseError>, WalkError> {
    let evidence = result?;
    let Some(site) = evidence.obligations().first() else {
        return Ok(None);
    };
    let error = errors
        .into_iter()
        .nth(*site.output())
        .ok_or(WalkError::ReceiptOrigin)?;
    Ok(Some(if site.is_secret() {
        error.inside_secret()
    } else {
        error
    }))
}

struct UnavailableSearch {
    policy: UnavailableSearchPolicy,
    errors: Vec<ParseError>,
}

impl Algebra<Inspect> for UnavailableSearch {
    type Output = Evidence<usize, ()>;

    fn scalar<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        scalar: ScopedScalar<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        scalar.finish(Evidence::empty())
    }

    fn list<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        items: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        items.inspect(self, Evidence::merge)
    }

    fn map<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        entries: Children<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        entries.inspect(self, Evidence::merge)
    }

    fn expression<P: ValueKind, B: BindingAddress>(
        &mut self,
        at: &At<'_>,
        expr: ExprChildren<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        let obligations = if self.policy.includes(at) {
            match &expr.children.parts {
                Parts::Unavailable {
                    name,
                    args: _,
                    reason,
                    source_line,
                } => {
                    let message = format!("{name}(): {reason}");
                    let error = match source_line.get() {
                        Some(line) => ParseError::InvalidExpression { line, message },
                        None => ParseError::UserFunctionError(message),
                    };
                    let handle = self.errors.len();
                    self.errors.push(error);
                    Some(handle)
                }
                Parts::List(_)
                | Parts::Map(_)
                | Parts::Coalesce {
                    value: _,
                    default: _,
                }
                | Parts::ModuleIf(_)
                | Parts::Arithmetic(_)
                | Parts::Interpolation(_)
                | Parts::Call {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | Parts::ResolvedCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | Parts::Access(_)
                | Parts::Evidence { value: _, edge: _ } => None,
            }
        } else {
            None
        };
        expr.with_evidence(obligations, []).inspect(self)
    }

    fn placeholder<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &At<'_>,
        leaf: ScopedLeaf<'_, Inspect, P, B>,
    ) -> Step<Inspect, Self::Output, P, B> {
        leaf.finish(Evidence::empty())
    }

    fn secret<P: ValueKind, B: BindingAddress>(
        &mut self,
        _: &SecretAt<'_>,
        child: SecretChild<'_, Inspect, P, B>,
    ) -> SecretStep<Inspect, Self::Output, P, B> {
        child.inspect(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_failure_sources_survive_parser_and_secret_error_boundaries() {
        let parser = ParseError::from(WalkError::Rejected).inside_secret();
        let secret =
            crate::value::SecretRehydrationError::from(WalkError::Rejected).inside_secret();
        for error in [
            &parser as &dyn std::error::Error,
            &secret as &dyn std::error::Error,
        ] {
            assert!(
                error
                    .source()
                    .is_some_and(|source| source.is::<WalkError>())
            );
        }
        assert!(matches!(parser, ParseError::ValueWalk(WalkError::Rejected)));
        assert!(matches!(
            secret,
            crate::value::SecretRehydrationError::ValueWalk(WalkError::Rejected)
        ));
    }

    #[test]
    fn unavailable_search_preserves_an_injected_walk_failure_type() {
        let failure = finish_unavailable_search(Err(WalkError::Rejected), Vec::new()).unwrap_err();
        let error: &dyn std::error::Error = &failure;
        assert_eq!(
            error.downcast_ref::<WalkError>(),
            Some(&WalkError::Rejected),
            "kernel failure lost its type: {failure:?}"
        );
    }

    fn inspect_4495_missing<P: ValueKind, B: BindingAddress>(
        line: usize,
        reason: &str,
    ) -> Value<P, B> {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![],
            reason: crate::resource::UnavailableReason::from_builtin_error(reason.into()),
            source_line: line.into(),
        })
    }

    fn inspect_4495_text(text: &str) -> Value {
        Value::Concrete(ConcreteValue::String(text.into()))
    }

    fn inspect_4495_call() -> Value {
        // The literal parameter keeps the resource argument's value check
        // pending, so the loader retains the callee and unused helper. Tests
        // below insert attempted failures at each excluded definition edge.
        let directory = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("providers.crn", "provider mock {}\n"),
            (
                "main.crn",
                "let origin = mock.test.Source { name = 'origin' }\nlet result = retained(origin.identifier)\n",
            ),
            ("exports.crn", "attributes { output: String = result }\n"),
            (
                "functions.crn",
                "fn unused(): String { 'unused' }\nfn retained(x: 'selected operand', fallback: String = 'default'): String {\n let local = 'local'\n x\n}\n",
            ),
        ] {
            std::fs::write(directory.path().join(name), source).unwrap();
        }
        let parsed = crate::config_loader::parse_directory(
            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap()
        .into_parsed()
        .resolve_root_for_test();
        let call = parsed.variables["result"].clone().into_expanded().unwrap();
        assert!(
            matches!(&call, Value::Deferred(DeferredValue::ResolvedFunctionCall { function, functions, .. })
            if function.name == "retained" && functions.contains_key("unused")),
            "directory fixture must retain its callee and unused helper, actual variant: {call:?}"
        );
        call
    }

    #[derive(Clone)]
    struct ExpectedUnavailable {
        line: usize,
        reason: &'static str,
        secret: bool,
        path: LeafPath,
        edges: Vec<ExpressionEdge>,
    }

    fn inspect_4495_expected(
        line: usize,
        reason: &'static str,
        secret: bool,
        edges: Vec<ExpressionEdge>,
    ) -> ExpectedUnavailable {
        ExpectedUnavailable {
            line,
            reason,
            secret,
            path: LeafPath::from_segments([]),
            edges,
        }
    }

    fn inspect_4495_assert(
        value: &Value,
        policy: UnavailableSearchPolicy,
        expected: &[ExpectedUnavailable],
    ) {
        let mut search = UnavailableSearch {
            policy,
            errors: Vec::new(),
        };
        let evidence = inspect(WalkRoot::syntactic(), value, &mut search)
            .unwrap()
            .into_output();
        // Excluded code/provenance must allocate neither handles nor errors.
        assert_eq!(
            search
                .errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|e| { format!("Invalid expression at line {}: env(): {}", e.line, e.reason) })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            evidence
                .obligations()
                .iter()
                .map(|site| (
                    *site.output(),
                    site.path.clone(),
                    site.expression_edges().to_vec(),
                    site.is_secret()
                ))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .enumerate()
                .map(|(i, e)| (i, e.path.clone(), e.edges.clone(), e.secret))
                .collect::<Vec<_>>()
        );
        assert!(evidence.receipts().is_empty());
        let first = finish_unavailable_search(Ok(evidence), search.errors).unwrap();
        match (first, expected.first()) {
            (None, None) => {}
            (Some(error), Some(expected)) => {
                let message = if expected.secret {
                    format!(
                        "Invalid expression at line {}: invalid value inside secret(...)",
                        expected.line
                    )
                } else {
                    format!(
                        "Invalid expression at line {}: env(): {}",
                        expected.line, expected.reason
                    )
                };
                assert_eq!(error.to_string(), message);
                if expected.secret {
                    assert!(
                        matches!(error, ParseError::InsideSecret { line } if line == expected.line)
                    );
                    assert!(!format!("{error:?}").contains(expected.reason));
                } else {
                    assert!(
                        matches!(error, ParseError::InvalidExpression { line, .. } if line == expected.line)
                    );
                }
            }
            _ => panic!("availability lost or invented its first error"),
        }
        assert_eq!(
            contains_unavailable(value, policy).unwrap(),
            !expected.is_empty()
        );

        // Exercise both actual saved-plan production callers, including the
        // cases in which preflight must allow hash verification to proceed.
        use crate::value::{
            SecretRehydrationError, redact_secrets_in_value, rehydrate_redacted_secrets,
            rehydrate_redacted_secrets_from_plaintext,
        };
        let result = match policy {
            UnavailableSearchPolicy::Required(_) => return,
            UnavailableSearchPolicy::SavedCandidate => {
                let saved = redact_secrets_in_value(&value.clone().into_secret()).unwrap();
                rehydrate_redacted_secrets_from_plaintext(&saved, value)
            }
            UnavailableSearchPolicy::SavedSecret => {
                let anchor = inspect_4495_text("anchor-4495").into_secret();
                let saved = redact_secrets_in_value(&Value::Concrete(ConcreteValue::Map(
                    indexmap::indexmap! {
                        "anchor".into() => anchor.clone(),
                        "candidate".into() => inspect_4495_text("saved public value"),
                    },
                )))
                .unwrap();
                let candidate = Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                    "anchor".into() => anchor,
                    "candidate".into() => value.clone(),
                }));
                rehydrate_redacted_secrets(&saved, &candidate)
            }
        };
        if expected.is_empty() {
            assert!(
                result.is_ok(),
                "excluded input blocked saved verification: {result:?}"
            );
        } else {
            let error = result.unwrap_err();
            assert!(matches!(
                error,
                SecretRehydrationError::EvaluationUnavailable
            ));
            assert_eq!(
                error.to_string(),
                "the current secret expression could not be evaluated"
            );
            assert_eq!(format!("{error:?}"), "EvaluationUnavailable");
        }
    }

    fn inspect_4495_definitions(policy: UnavailableSearchPolicy) {
        for location in ["default", "local", "body", "captured"] {
            let mut call = inspect_4495_call();
            let Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function,
                functions,
                ..
            }) = &mut call
            else {
                unreachable!()
            };
            // Secret definitions must not mask an unrelated public operand.
            let failure = || inspect_4495_missing(5, "private-definition-4495").into_secret();
            match location {
                "default" => Arc::make_mut(function).params[1].default = Some(failure()),
                "local" => Arc::make_mut(function).local_lets[0].1 = failure(),
                "body" => Arc::make_mut(function).body.value = failure(),
                "captured" => {
                    // Only the unused captured helper contains unavailable input.
                    Arc::make_mut(functions)
                        .get_mut("unused")
                        .unwrap()
                        .body
                        .value = failure()
                }
                _ => unreachable!(),
            }
            inspect_4495_assert(&call, policy, &[]);
            let Value::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = &mut call
            else {
                unreachable!()
            };
            args.clear();
            args.push(FunctionArgument::from_value(inspect_4495_missing(
                17,
                "first-primary-4495",
            )));
            args.push(FunctionArgument::from_value(
                inspect_4495_missing(19, "private-second-primary-4495").into_secret(),
            ));
            let mut expected = Vec::new();
            if !matches!(policy, UnavailableSearchPolicy::SavedSecret) {
                expected.push(inspect_4495_expected(
                    17,
                    "first-primary-4495",
                    false,
                    vec![
                        ExpressionEdge::Argument(0),
                        ExpressionEdge::ArgumentValue(0),
                    ],
                ));
            }
            expected.push(inspect_4495_expected(
                19,
                "private-second-primary-4495",
                true,
                vec![
                    ExpressionEdge::Argument(1),
                    ExpressionEdge::ArgumentValue(0),
                ],
            ));
            inspect_4495_assert(&call, policy, &expected);
        }
    }

    fn inspect_4495_auxiliary(policy: UnavailableSearchPolicy) {
        let mut call = inspect_4495_call();
        let Value::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = &mut call else {
            unreachable!()
        };
        *args = vec![FunctionArgument::with_auxiliary_for_inspect_test(
            inspect_4495_text("selected operand"),
            inspect_4495_missing(3, "private-auxiliary-4495").into_secret(),
        )];
        inspect_4495_assert(&call, policy, &[]);
        let Value::Deferred(DeferredValue::ResolvedFunctionCall { args, .. }) = &mut call else {
            unreachable!()
        };
        args.push(FunctionArgument::from_value(
            inspect_4495_missing(23, "private-primary-4495").into_secret(),
        ));
        inspect_4495_assert(
            &call,
            policy,
            &[inspect_4495_expected(
                23,
                "private-primary-4495",
                true,
                vec![
                    ExpressionEdge::Argument(1),
                    ExpressionEdge::ArgumentValue(0),
                ],
            )],
        );
    }

    fn inspect_4495_nested_order(policy: UnavailableSearchPolicy) {
        for reversed in [false, true] {
            let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("private-prefix-4495".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::Secret(
                    Box::new(inspect_4495_missing(31, "private-nested-4495").into_secret()).into(),
                ))),
                InterpolationPart::Expr(inspect_4495_missing(32, "public-nested-4495")),
            ]));
            let mut entries = vec![
                ("a".into(), interpolation),
                (
                    "z".into(),
                    inspect_4495_missing(9, "earlier-source-line-later-key-4495").into_secret(),
                ),
            ];
            if reversed {
                entries.reverse();
            }
            let value = Value::Concrete(ConcreteValue::Map(entries.into_iter().collect()));
            let mut expected = vec![inspect_4495_expected(
                31,
                "private-nested-4495",
                true,
                vec![ExpressionEdge::Interpolation(1)],
            )];
            expected[0].path = LeafPath::from_segments([LeafPathSegment::MapKey("a".into())]);
            if !matches!(policy, UnavailableSearchPolicy::SavedSecret) {
                let mut public = inspect_4495_expected(
                    32,
                    "public-nested-4495",
                    false,
                    vec![ExpressionEdge::Interpolation(2)],
                );
                public.path = expected[0].path.clone();
                expected.push(public);
            }
            let mut last =
                inspect_4495_expected(9, "earlier-source-line-later-key-4495", true, vec![]);
            last.path = LeafPath::from_segments([LeafPathSegment::MapKey("z".into())]);
            expected.push(last);
            // Map visits preserve insertion order, including the first error.
            if reversed {
                expected.rotate_right(1);
            }
            inspect_4495_assert(&value, policy, &expected);
        }
    }

    fn inspect_4495_coalesce(policy: UnavailableSearchPolicy) {
        let missing = inspect_4495_missing(41, "private-fallback-4495").into_secret();
        for primary in [
            inspect_4495_text("selected"),
            Value::resource_ref("origin", "identifier", vec![]).resolve_root_for_test(),
        ] {
            let value = Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(primary),
                default: Box::new(missing.clone()),
            });
            let expected = if matches!(policy, UnavailableSearchPolicy::Required(_)) {
                vec![]
            } else {
                vec![inspect_4495_expected(
                    41,
                    "private-fallback-4495",
                    true,
                    vec![ExpressionEdge::CoalesceDefault],
                )]
            };
            inspect_4495_assert(&value, policy, &expected);
        }
        // After selection the default is the demanded value, with its own line.
        inspect_4495_assert(
            &missing,
            policy,
            &[inspect_4495_expected(
                41,
                "private-fallback-4495",
                true,
                vec![],
            )],
        );
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(inspect_4495_missing(43, "primary-before-fallback-4495").into_secret()),
            default: Box::new(missing),
        });
        let mut expected = vec![inspect_4495_expected(
            43,
            "primary-before-fallback-4495",
            true,
            vec![ExpressionEdge::CoalesceValue],
        )];
        if !matches!(policy, UnavailableSearchPolicy::Required(_)) {
            expected.push(inspect_4495_expected(
                41,
                "private-fallback-4495",
                true,
                vec![ExpressionEdge::CoalesceDefault],
            ));
        }
        inspect_4495_assert(&value, policy, &expected);
    }

    macro_rules! inspect_4495_pins {
        ($name:ident, $policy:expr, $case:ident) => {
            #[test]
            fn $name() {
                $case($policy);
            }
        };
    }
    inspect_4495_pins!(
        inspect_4495_required_definition_and_primary_pins,
        UnavailableSearchPolicy::Required(crate::config_loader::strict_resolution_context()),
        inspect_4495_definitions
    );
    inspect_4495_pins!(
        inspect_4495_saved_candidate_definition_and_primary_pins,
        UnavailableSearchPolicy::SavedCandidate,
        inspect_4495_definitions
    );
    inspect_4495_pins!(
        inspect_4495_saved_secret_definition_and_primary_pins,
        UnavailableSearchPolicy::SavedSecret,
        inspect_4495_definitions
    );
    inspect_4495_pins!(
        inspect_4495_required_auxiliary_pins,
        UnavailableSearchPolicy::Required(crate::config_loader::strict_resolution_context()),
        inspect_4495_auxiliary
    );
    inspect_4495_pins!(
        inspect_4495_saved_candidate_auxiliary_pins,
        UnavailableSearchPolicy::SavedCandidate,
        inspect_4495_auxiliary
    );
    inspect_4495_pins!(
        inspect_4495_saved_secret_auxiliary_pins,
        UnavailableSearchPolicy::SavedSecret,
        inspect_4495_auxiliary
    );
    inspect_4495_pins!(
        inspect_4495_required_nested_order_pins,
        UnavailableSearchPolicy::Required(crate::config_loader::strict_resolution_context()),
        inspect_4495_nested_order
    );
    inspect_4495_pins!(
        inspect_4495_saved_candidate_nested_order_pins,
        UnavailableSearchPolicy::SavedCandidate,
        inspect_4495_nested_order
    );
    inspect_4495_pins!(
        inspect_4495_saved_secret_nested_order_pins,
        UnavailableSearchPolicy::SavedSecret,
        inspect_4495_nested_order
    );
    inspect_4495_pins!(
        inspect_4495_required_coalesce_pins,
        UnavailableSearchPolicy::Required(crate::config_loader::strict_resolution_context()),
        inspect_4495_coalesce
    );
    inspect_4495_pins!(
        inspect_4495_saved_candidate_coalesce_pins,
        UnavailableSearchPolicy::SavedCandidate,
        inspect_4495_coalesce
    );
    inspect_4495_pins!(
        inspect_4495_saved_secret_coalesce_pins,
        UnavailableSearchPolicy::SavedSecret,
        inspect_4495_coalesce
    );

    fn inspect_4495_saved_directory_input(policy: UnavailableSearchPolicy) {
        let directory = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("providers.crn", "provider mock {}\n"),
            (
                "main.crn",
                "// directory input fixture\nlet result = secret(\"prefix-${env('CARINA_INSPECT_4495_PRIVATE')}\")\n",
            ),
            ("exports.crn", "attributes { output: String = result }\n"),
            (
                "functions.crn",
                "fn unused(): String { env('CARINA_INSPECT_4495_UNUSED') }\n",
            ),
        ] {
            std::fs::write(directory.path().join(name), source).unwrap();
        }
        let loaded = crate::config_loader::load_configuration_for_saved_rehydration(
            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let parsed = loaded.parsed.into_parsed().resolve_root_for_test();
        let value = parsed.variables["result"].clone().into_expanded().unwrap();
        let value = &value;
        inspect_4495_assert(
            value,
            policy,
            &[inspect_4495_expected(
                2,
                crate::resource::INSIDE_SECRET_MESSAGE,
                true,
                vec![ExpressionEdge::Interpolation(1)],
            )],
        );
        for rendered in [
            format!("{value:?}"),
            serde_json::to_string(&crate::value::redact_secrets_in_value(value).unwrap()).unwrap(),
        ] {
            assert!(!rendered.contains("CARINA_INSPECT_4495_PRIVATE"));
        }
    }

    #[test]
    fn inspect_4495_saved_candidate_directory_attempted_input_pin() {
        inspect_4495_saved_directory_input(UnavailableSearchPolicy::SavedCandidate);
    }

    #[test]
    fn inspect_4495_saved_secret_directory_attempted_input_pin() {
        inspect_4495_saved_directory_input(UnavailableSearchPolicy::SavedSecret);
    }

    fn inspect_4495_preorder(policy: UnavailableSearchPolicy) {
        let public = inspect_4495_missing(59, "public-with-unrelated-secret-anchor-4495");
        let expected = if matches!(policy, UnavailableSearchPolicy::SavedSecret) {
            vec![]
        } else {
            vec![inspect_4495_expected(
                59,
                "public-with-unrelated-secret-anchor-4495",
                false,
                vec![],
            )]
        };
        inspect_4495_assert(&public, policy, &expected);
        let value = Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![FunctionArgument::from_value(
                inspect_4495_missing(3, "private-child-4495").into_secret(),
            )],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "outer-before-child-4495".into(),
            ),
            source_line: 61.into(),
        });
        let mut expected = Vec::new();
        if !matches!(policy, UnavailableSearchPolicy::SavedSecret) {
            expected.push(inspect_4495_expected(
                61,
                "outer-before-child-4495",
                false,
                vec![],
            ));
        }
        expected.push(inspect_4495_expected(
            3,
            "private-child-4495",
            true,
            vec![
                ExpressionEdge::Argument(0),
                ExpressionEdge::ArgumentValue(0),
            ],
        ));
        inspect_4495_assert(&value, policy, &expected);
    }

    inspect_4495_pins!(
        inspect_4495_required_expression_preorder_pins,
        UnavailableSearchPolicy::Required(crate::config_loader::strict_resolution_context()),
        inspect_4495_preorder
    );
    inspect_4495_pins!(
        inspect_4495_saved_candidate_expression_preorder_pins,
        UnavailableSearchPolicy::SavedCandidate,
        inspect_4495_preorder
    );
    inspect_4495_pins!(
        inspect_4495_saved_secret_expression_preorder_pins,
        UnavailableSearchPolicy::SavedSecret,
        inspect_4495_preorder
    );
}
