use std::collections::HashSet;

use crate::provider::{Provider, ProviderResult};
use crate::resource::ResourceId;
use crate::wait::BindingPattern;
use crate::wait::predicate::AttrPath;

/// Resolve satisfier hints from the provider against the plan's known bindings,
/// and return additional explicit dependencies for a wait on `target_id.attr_path`.
pub fn satisfier_augmentation(
    provider: &dyn Provider,
    target_id: &ResourceId,
    attr_path: &AttrPath,
    known_bindings: &HashSet<String>,
) -> ProviderResult<HashSet<String>> {
    let mut additional = HashSet::new();

    for hint in provider.satisfier_hint(target_id, attr_path)? {
        match hint {
            BindingPattern::Exact(name) => {
                if known_bindings.contains(&name) {
                    additional.insert(name);
                }
            }
            BindingPattern::ForLoopChildren { base } => {
                let prefix = format!("{base}[");
                additional.extend(
                    known_bindings
                        .iter()
                        .filter(|name| name.starts_with(&prefix))
                        .cloned(),
                );
            }
            BindingPattern::AttributeMatch {
                resource_type,
                attr,
                from,
            } => {
                tracing::debug!(
                    target = "carina_core::wait::augment",
                    %resource_type,
                    attr = ?attr,
                    from = ?from,
                    "skipping state-dependent wait satisfier hint"
                );
            }
        }
    }

    Ok(additional)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HintProvider {
        hints: Vec<BindingPattern>,
        failure: Option<&'static str>,
    }

    impl Provider for HintProvider {
        fn name(&self) -> &str {
            "hint"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: crate::provider::ReadRequest,
        ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<crate::resource::State>>
        {
            Box::pin(async { panic!("unexpected read") })
        }

        fn read_data_source(
            &self,
            _resource: &crate::provider::ProviderReadyDataSource,
        ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<crate::resource::State>>
        {
            Box::pin(async { panic!("unexpected read_data_source") })
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: crate::provider::CreateRequest,
        ) -> crate::provider::BoxFuture<
            '_,
            crate::provider::ProviderResult<crate::provider::CreateOutcome>,
        > {
            Box::pin(async { panic!("unexpected create") })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: crate::provider::UpdateRequest,
        ) -> crate::provider::BoxFuture<
            '_,
            crate::provider::ProviderResult<crate::provider::UpdateOutcome>,
        > {
            Box::pin(async { panic!("unexpected update") })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: crate::provider::DeleteRequest,
        ) -> crate::provider::BoxFuture<'_, crate::provider::ProviderResult<()>> {
            Box::pin(async { panic!("unexpected delete") })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> crate::provider::ProviderResult<Vec<String>> {
            Ok(Vec::new())
        }

        fn satisfier_hint(
            &self,
            _target_id: &ResourceId,
            _attr_path: &AttrPath,
        ) -> crate::provider::ProviderResult<Vec<BindingPattern>> {
            if let Some(message) = self.failure {
                return Err(crate::provider::ProviderError::internal(message));
            }
            Ok(self.hints.clone())
        }
    }

    fn augment(hints: Vec<BindingPattern>, known: &[&str]) -> HashSet<String> {
        let provider = HintProvider {
            hints,
            failure: None,
        };
        let known_bindings = known.iter().map(|name| (*name).to_string()).collect();
        satisfier_augmentation(
            &provider,
            &ResourceId::with_identity("acm.Certificate", "cert"),
            &AttrPath::single("status"),
            &known_bindings,
        )
        .expect("test provider hints should succeed")
    }

    #[test]
    fn exact_hint_included_when_known() {
        assert_eq!(
            augment(vec![BindingPattern::Exact("alb".to_string())], &["alb"]),
            HashSet::from(["alb".to_string()])
        );
    }

    #[test]
    fn exact_hint_skipped_when_missing() {
        assert!(augment(vec![BindingPattern::Exact("missing".to_string())], &["alb"]).is_empty());
    }

    #[test]
    fn for_loop_children_includes_matching_children() {
        assert_eq!(
            augment(
                vec![BindingPattern::ForLoopChildren {
                    base: "validation_records".to_string(),
                }],
                &[
                    "validation_records[0]",
                    "validation_records[1]",
                    "unrelated_binding",
                ],
            ),
            HashSet::from([
                "validation_records[0]".to_string(),
                "validation_records[1]".to_string(),
            ])
        );
    }

    #[test]
    fn attribute_match_is_deferred() {
        assert!(
            augment(
                vec![BindingPattern::AttributeMatch {
                    resource_type: "route53.Record".to_string(),
                    attr: AttrPath::single("name"),
                    from: AttrPath::single("validation_name"),
                }],
                &["route53_record"],
            )
            .is_empty()
        );
    }

    #[test]
    fn provider_failure_is_propagated() {
        let provider = HintProvider {
            hints: Vec::new(),
            failure: Some("intentional satisfier hint failure"),
        };

        let error = satisfier_augmentation(
            &provider,
            &ResourceId::with_identity("acm.Certificate", "cert"),
            &AttrPath::single("status"),
            &HashSet::new(),
        )
        .expect_err("provider hint failures must abort augmentation");

        assert!(
            error
                .to_string()
                .contains("intentional satisfier hint failure"),
            "the provider error must be preserved: {error}"
        );
    }
}
