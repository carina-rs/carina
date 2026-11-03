//! Thread-local work counters for analysis reuse regression tests.

use std::cell::Cell;

thread_local! {
    static TREE_BUILDS: Cell<usize> = const { Cell::new(0) };
    static MODULE_CALL_CHECKS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn tree_built() {
    TREE_BUILDS.set(TREE_BUILDS.get() + 1);
}

pub(crate) fn module_calls_checked() {
    MODULE_CALL_CHECKS.set(MODULE_CALL_CHECKS.get() + 1);
}

#[derive(Debug)]
pub struct AnalysisWork {
    pub tree_builds: usize,
    pub module_call_checks: usize,
}

pub fn measure<T>(operation: impl FnOnce() -> T) -> (T, AnalysisWork) {
    let trees = TREE_BUILDS.get();
    let calls = MODULE_CALL_CHECKS.get();
    let result = operation();
    (
        result,
        AnalysisWork {
            tree_builds: TREE_BUILDS.get() - trees,
            module_call_checks: MODULE_CALL_CHECKS.get() - calls,
        },
    )
}
