//! Test-only, per-thread verification of both definition memos.
use std::cell::Cell;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static BYPASSED: Cell<bool> = const { Cell::new(false) };
    static BINDING_HITS: Cell<usize> = const { Cell::new(0) };
    static TRANSFORM_HITS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn enabled() -> bool {
    ENABLED.get() && !bypassed()
}
pub(crate) fn bypassed() -> bool {
    BYPASSED.get()
}
pub(crate) fn binding_hit() {
    BINDING_HITS.set(BINDING_HITS.get() + 1);
}
pub(crate) fn transform_hit() {
    TRANSFORM_HITS.set(TRANSFORM_HITS.get() + 1);
}

pub(crate) struct ReferenceWalk(bool);
impl ReferenceWalk {
    pub(crate) fn enter() -> Self {
        Self(BYPASSED.replace(true))
    }
}
impl Drop for ReferenceWalk {
    fn drop(&mut self) {
        BYPASSED.set(self.0);
    }
}

pub(crate) fn verify<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Restore(bool, usize, usize);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.set(self.0);
            BINDING_HITS.set(self.1);
            TRANSFORM_HITS.set(self.2);
        }
    }
    let _restore = Restore(
        ENABLED.replace(true),
        BINDING_HITS.replace(0),
        TRANSFORM_HITS.replace(0),
    );
    let result = run();
    (result, BINDING_HITS.get(), TRANSFORM_HITS.get())
}
