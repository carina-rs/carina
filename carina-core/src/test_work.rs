//! Deterministic work probes, compiled only for tests or the test-utils feature.

use std::cell::Cell;

#[derive(Clone, Copy)]
pub enum Operation {
    ColumnBytes,
    PositionIndexBytes,
    PositionQuery,
    PipeHintCharacter,
    ProjectResource,
    LintScanBytes,
    LintSearchBytes,
    LintIndexStep,
    LintWarningMatch,
    LintPositionBytes,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Work([usize; 10]);

impl Work {
    pub fn get(self, operation: Operation) -> usize {
        self.0[operation as usize]
    }
}

thread_local! {
    static WORK: Cell<Work> = const { Cell::new(Work([0; 10])) };
}

pub fn record(operation: Operation, amount: usize) {
    WORK.with(|work| {
        let mut value = work.get();
        value.0[operation as usize] += amount;
        work.set(value);
    });
}

pub fn measure<T>(run: impl FnOnce() -> T) -> (T, Work) {
    WORK.with(|work| {
        let saved = work.replace(Work::default());
        let value = run();
        let measured = work.get();
        let mut total = saved;
        for (total, measured) in total.0.iter_mut().zip(measured.0) {
            *total += measured;
        }
        work.set(total);
        (value, measured)
    })
}
