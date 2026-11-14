//! Per-analysis observations; unavailable in the default production build.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionPathCoverage {
    pub reached_admission: u64,
    pub skipped_missing_schema: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpandedDeclineReason {
    MissingResource,
    MissingSchema,
    MissingReport,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionCoverage {
    pub relative: AdmissionPathCoverage,
    pub expanded: AdmissionPathCoverage,
    pub expanded_declined_fell_back: BTreeMap<ExpandedDeclineReason, u64>,
    pub expanded_declined_skipped_module_resource: BTreeMap<ExpandedDeclineReason, u64>,
}

pub fn with_admission_coverage<T>(analyze: impl FnOnce() -> T) -> (T, AdmissionCoverage) {
    struct Restore(Option<Observation>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(ACTIVE.with(|active| active.replace(Some(Observation::default()))));
    let result = analyze();
    let coverage = ACTIVE.with(|active| active.borrow_mut().take().unwrap().coverage);
    (result, coverage)
}

#[derive(Default)]
struct Observation {
    coverage: AdmissionCoverage,
    decline: Option<ExpandedDeclineReason>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Observation>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy)]
pub(crate) enum Path {
    Relative,
    Expanded,
}

fn update(action: impl FnOnce(&mut Observation)) {
    ACTIVE.with(|active| {
        if let Some(observation) = active.borrow_mut().as_mut() {
            action(observation);
        }
    });
}

pub(crate) fn reached(path: Path) {
    update(|observation| match path {
        Path::Relative => observation.coverage.relative.reached_admission += 1,
        Path::Expanded => observation.coverage.expanded.reached_admission += 1,
    });
}

pub(crate) fn missing_schema(path: Path, provider: &str) {
    update(|observation| {
        let path = match path {
            Path::Relative => &mut observation.coverage.relative,
            Path::Expanded => &mut observation.coverage.expanded,
        };
        *path
            .skipped_missing_schema
            .entry(provider.into())
            .or_default() += 1;
    });
}

pub(crate) fn declined(reason: ExpandedDeclineReason) {
    update(|observation| observation.decline = Some(reason));
}

pub(crate) fn declined_outcome(fell_back: bool) {
    update(|observation| {
        if let Some(reason) = observation.decline.take() {
            let counts = if fell_back {
                &mut observation.coverage.expanded_declined_fell_back
            } else {
                &mut observation
                    .coverage
                    .expanded_declined_skipped_module_resource
            };
            *counts.entry(reason).or_default() += 1;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_restores_outer_scope_on_panic_and_does_not_accumulate() {
        let (_, coverage) = with_admission_coverage(|| {
            reached(Path::Relative);
            let result = std::panic::catch_unwind(|| {
                with_admission_coverage(|| {
                    reached(Path::Expanded);
                    panic!("analysis panic");
                })
            });
            assert!(result.is_err());
            reached(Path::Relative);
        });
        assert_eq!(coverage.relative.reached_admission, 2);
        assert_eq!(coverage.expanded.reached_admission, 0);
        assert_eq!(
            with_admission_coverage(|| ()).1,
            AdmissionCoverage::default()
        );
    }
}

#[cfg(test)]
mod decline_scope_tests {
    use super::*;

    #[test]
    fn pending_decline_reason_survives_a_nested_analysis_panic() {
        let (_, coverage) = with_admission_coverage(|| {
            declined(ExpandedDeclineReason::MissingReport);
            let inner = std::panic::catch_unwind(|| {
                with_admission_coverage(|| {
                    declined(ExpandedDeclineReason::MissingResource);
                    declined_outcome(true);
                    panic!("nested analysis panic");
                })
            });
            assert!(inner.is_err());
            declined_outcome(false);
        });
        assert!(coverage.expanded_declined_fell_back.is_empty());
        assert_eq!(
            coverage.expanded_declined_skipped_module_resource,
            BTreeMap::from([(ExpandedDeclineReason::MissingReport, 1)])
        );
    }
}
