//! Replacement reasons ordered by system identity, independently of display text.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::non_empty::NonEmptyVec;
use crate::provider_text::{DiagnosticText, DiagnosticTextBuilder};
use crate::resource::ResolvedResourceId;

/// A resource or deferred effect that requires a producer to be created first.
/// Deferred templates supply their system-assigned `deferred_effect_id`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Dependent(ResolvedResourceId);

impl Dependent {
    pub fn new(id: ResolvedResourceId) -> Self {
        Self(id)
    }

    /// The sole diagnostic spelling for both concrete and deferred dependents.
    pub fn render(&self) -> String {
        self.0.to_string()
    }
}

/// A non-empty ordered set. The existing non-empty owner retains the canonical
/// sorted, deduplicated sequence; no API exposes mutable sequence access.
/// Decoding requires that sequence to already be strictly increasing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Dependent>", into = "Vec<Dependent>")]
pub struct Dependents(NonEmptyVec<Dependent>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("empty ordered set of replacement dependents")]
pub struct EmptyDependents;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DependentsDecodeError {
    #[error(transparent)]
    Empty(#[from] EmptyDependents),
    #[error("replacement dependents must be strictly increasing by system identity")]
    NotStrictlyIncreasing,
}

impl Dependents {
    pub fn try_from_iter(
        dependents: impl IntoIterator<Item = Dependent>,
    ) -> Result<Self, EmptyDependents> {
        Self::try_from(dependents.into_iter().collect::<BTreeSet<_>>())
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Dependent> {
        self.0.iter()
    }

    pub(crate) fn extend(&mut self, dependents: impl IntoIterator<Item = Dependent>) {
        *self = Self::try_from_iter(self.iter().cloned().chain(dependents))
            .expect("union retains the existing non-empty set");
    }

    /// The full set is persisted. Diagnostics can grow with the number of
    /// resource/deferred identities in the graph, so stream into the existing
    /// 8 KiB diagnostic budget, including context and the truncation marker.
    pub(crate) fn diagnostic(&self, prefix: &str) -> DiagnosticText {
        let mut text = DiagnosticTextBuilder::default();
        text.push_str(prefix);
        text.push_str("create_before_destroy required by ");
        text.push_str(if self.iter().len() == 1 {
            "dependent "
        } else {
            "dependents "
        });
        for (index, dependent) in self.iter().enumerate() {
            if index != 0 {
                text.push_str(", ");
            }
            text.push_str("`");
            text.push_inline_str(&dependent.render());
            text.push_str("`");
        }
        text.finish()
    }
}

impl From<Dependent> for Dependents {
    fn from(dependent: Dependent) -> Self {
        Self(NonEmptyVec::from_vec(vec![dependent]).expect("singleton is non-empty"))
    }
}

impl TryFrom<BTreeSet<Dependent>> for Dependents {
    type Error = EmptyDependents;

    fn try_from(dependents: BTreeSet<Dependent>) -> Result<Self, Self::Error> {
        NonEmptyVec::from_vec(dependents.into_iter().collect())
            .map(Self)
            .ok_or(EmptyDependents)
    }
}

impl TryFrom<Vec<Dependent>> for Dependents {
    type Error = DependentsDecodeError;

    fn try_from(dependents: Vec<Dependent>) -> Result<Self, Self::Error> {
        let dependents = NonEmptyVec::from_vec(dependents).ok_or(EmptyDependents)?;
        if dependents
            .as_slice()
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(DependentsDecodeError::NotStrictlyIncreasing);
        }
        Ok(Self(dependents))
    }
}

impl From<Dependents> for Vec<Dependent> {
    fn from(dependents: Dependents) -> Self {
        dependents.0.into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dependent(identity: &str, route: Option<&str>) -> Dependent {
        Dependent::new(ResolvedResourceId::with_provider_identity(
            "mock",
            "Consumer",
            crate::resource::ResourceIdentity::new(identity),
            route.map(str::to_owned),
        ))
    }

    #[test]
    fn issue_4418_non_empty_set_orders_by_complete_identity_and_rejects_empty_decode() {
        let a = dependent("a", None);
        let route_a = dependent("a", Some("a"));
        let route_z = dependent("a", Some("z"));
        let z = dependent("z", None);
        let set = Dependents::try_from_iter([
            z.clone(),
            route_z.clone(),
            a.clone(),
            route_a.clone(),
            a.clone(),
        ])
        .unwrap();
        assert_eq!(
            set.iter().cloned().collect::<Vec<_>>(),
            vec![a, route_a, route_z, z]
        );
        let encoded = serde_json::to_value(&set).unwrap();
        assert_eq!(serde_json::from_value::<Dependents>(encoded).unwrap(), set);
        assert_eq!(Dependents::try_from_iter([]), Err(EmptyDependents));
        assert_eq!(
            Dependents::try_from(Vec::<Dependent>::new()),
            Err(DependentsDecodeError::Empty(EmptyDependents))
        );
        let error = serde_json::from_str::<Dependents>("[]").unwrap_err();
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(error.to_string().contains(&EmptyDependents.to_string()));
        assert!(
            serde_json::from_str::<Dependents>(r#""binding-name""#).is_err(),
            "there is no legacy string decoder"
        );
    }

    #[test]
    fn issue_4418_decode_rejects_duplicate_dependents() {
        let a = dependent("a", None);
        let input = vec![a.clone(), a];
        let wire = serde_json::to_value(&input).unwrap();
        let error = serde_json::from_value::<Dependents>(wire)
            .expect_err("duplicate dependents must be rejected, not deduplicated");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(error.to_string().contains("strictly increasing"), "{error}");
        assert_eq!(
            Dependents::try_from(input),
            Err(DependentsDecodeError::NotStrictlyIncreasing)
        );
    }

    #[test]
    fn issue_4418_decode_rejects_unsorted_dependents() {
        let input = vec![dependent("z", None), dependent("a", None)];
        let wire = serde_json::to_value(&input).unwrap();
        let error = serde_json::from_value::<Dependents>(wire)
            .expect_err("unsorted dependents must be rejected, not reordered");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(error.to_string().contains("strictly increasing"), "{error}");
        assert_eq!(
            Dependents::try_from(input),
            Err(DependentsDecodeError::NotStrictlyIncreasing)
        );
    }

    #[test]
    fn issue_4418_diagnostic_budget_does_not_truncate_saved_dependents() {
        let set = Dependents::try_from_iter(
            (0..2000).map(|i| dependent(&format!("consumer-{i:04}-あいうえお"), None)),
        )
        .unwrap();
        let rendered = set.diagnostic("");
        assert!(rendered.len() <= crate::provider_text::MAX_PROVIDER_TEXT_BYTES);
        assert!(rendered.starts_with(
            "create_before_destroy required by dependents `mock.Consumer.consumer-0000-あいうえお`"
        ));
        assert!(rendered.contains("… (truncated, "));
        let wire = serde_json::to_value(&set).unwrap();
        assert_eq!(wire.as_array().unwrap().len(), 2000);
        assert_eq!(serde_json::from_value::<Dependents>(wire).unwrap(), set);
    }
}
