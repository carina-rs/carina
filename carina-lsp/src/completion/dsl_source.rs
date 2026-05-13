//! Source-input abstraction for DSL text-scanning helpers.
//!
//! Carina configurations are directory-scoped: a `let` binding in
//! `main.crn` is routinely referenced from `exports.crn`,
//! `backend.crn`, etc. Historically text-scanning helpers
//! (`extract_resource_bindings`, `extract_let_bindings`,
//! `extract_argument_parameters`) took a bare `&str`, and the burden
//! of gathering sibling `.crn` files sat on every caller. That's how
//! #2043 / #2120 / #2122 shipped single-file blind spots — the signature
//! gave no hint that the sibling scan was required.
//!
//! [`DslSource`] forces the caller to make the choice explicit:
//!
//! - [`DslSource::BufferOnly`] — deliberate single-file scan. Grep for
//!   this variant during review to ask "is single-file really
//!   intended here?"
//! - [`DslSource::DirectoryScoped`] — pre-merged buffer + sibling
//!   `.crn` content. Built once per LSP request via
//!   [`DslSource::resolve_directory`]; every helper called under the
//!   same `src` then shares the same `&str`, so the disk read is
//!   single-shot even though multiple helpers scan the source.

use std::path::Path;

/// Explicit contract for what a text-scanning helper is allowed to see.
///
/// `Copy` so helpers can take it by value without ceremony. Both
/// variants hold a borrowed `&str`; the directory variant's `&str`
/// comes from a `String` owned by the caller's stack (see
/// [`DslSource::resolve_directory`]).
#[derive(Clone, Copy)]
pub(crate) enum DslSource<'a> {
    /// Scan only the given buffer. Use when the feature is genuinely
    /// buffer-local (e.g. position-dependent lookup on the current
    /// line). Cross-sibling semantics are a bug here — choose
    /// [`DslSource::DirectoryScoped`] if in doubt.
    BufferOnly(&'a str),
    /// Scan the pre-merged text containing the buffer **and** every
    /// sibling `.crn` under the original `base_path`. Construct via
    /// [`DslSource::resolve_directory`] so the sibling read happens
    /// exactly once per request.
    DirectoryScoped { merged: &'a str },
}

impl<'a> DslSource<'a> {
    /// Build a `DirectoryScoped` source, reading sibling `.crn` files
    /// into `storage` exactly once. The returned `DslSource` borrows
    /// from `storage`, so `storage` must outlive every helper call.
    ///
    /// Typical pattern at an LSP completion entry point:
    ///
    /// ```ignore
    /// let mut src_buf = String::new();
    /// let src = DslSource::resolve_directory(&text, base_path, &mut src_buf);
    /// // pass `src` to as many helpers as needed — no extra disk reads.
    /// ```
    pub(crate) fn resolve_directory(
        buffer: &'a str,
        base_path: Option<&Path>,
        storage: &'a mut String,
    ) -> Self {
        storage.clear();
        storage.push_str(buffer);
        if !storage.ends_with('\n') {
            storage.push('\n');
        }
        storage.push_str(&read_sibling_crn(base_path));
        DslSource::DirectoryScoped { merged: storage }
    }

    /// Borrow the scanned text. Zero-alloc for both variants.
    pub(crate) fn merged_text(&self) -> &'a str {
        match *self {
            DslSource::BufferOnly(buffer) => buffer,
            DslSource::DirectoryScoped { merged } => merged,
        }
    }
}

/// Concatenate every `.crn` file in `base_path` into one string.
/// Duplicates with the buffer are expected — callers dedupe by
/// binding name.
///
/// Files are read in lexicographic path order via
/// [`carina_core::config_loader::find_crn_files_in_dir`], matching the
/// canonical loader's ordering contract (#2449, #2855). Iterating
/// `std::fs::read_dir` directly would yield filesystem-dependent order,
/// which makes downstream merged-text scanning non-deterministic.
pub(crate) fn read_sibling_crn(base_path: Option<&Path>) -> String {
    let Some(base) = base_path else {
        return String::new();
    };
    let Ok(files) = carina_core::config_loader::find_crn_files_in_dir(base) else {
        return String::new();
    };
    let mut out = String::new();
    for path in files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            out.push_str(&content);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_only_returns_buffer_unchanged() {
        let src = DslSource::BufferOnly("let a = 1\n");
        assert_eq!(src.merged_text(), "let a = 1\n");
    }

    #[test]
    fn resolve_directory_without_base_path_is_buffer_only() {
        let mut storage = String::new();
        let src = DslSource::resolve_directory("let a = 1\n", None, &mut storage);
        assert_eq!(src.merged_text(), "let a = 1\n");
    }

    #[test]
    fn read_sibling_crn_orders_files_lexicographically() {
        // Regression for #2855: read_sibling_crn previously iterated
        // `std::fs::read_dir` directly, so the merged text reflected
        // filesystem discovery order. Names are intentionally chosen so
        // creation order ("z", "a", "m") differs from lexicographic order
        // ("a", "m", "z").
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::write(base.join("z.crn"), "let z = 26\n").unwrap();
        std::fs::write(base.join("a.crn"), "let a = 1\n").unwrap();
        std::fs::write(base.join("m.crn"), "let m = 13\n").unwrap();

        let merged = read_sibling_crn(Some(base));
        let pos_a = merged.find("let a = 1").expect("a.crn missing");
        let pos_m = merged.find("let m = 13").expect("m.crn missing");
        let pos_z = merged.find("let z = 26").expect("z.crn missing");
        assert!(
            pos_a < pos_m && pos_m < pos_z,
            "sibling .crn files should be merged in lexicographic order. Got: {:?}",
            merged
        );
    }

    #[test]
    fn resolve_directory_includes_sibling_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        std::fs::write(base.join("main.crn"), "let a = 1\n").unwrap();
        std::fs::write(base.join("exports.crn"), "let b = 2\n").unwrap();
        let buffer = "let c = 3\n"; // unsaved buffer, not on disk
        let mut storage = String::new();
        let src = DslSource::resolve_directory(buffer, Some(base), &mut storage);
        let merged = src.merged_text();
        assert!(
            merged.contains("let a = 1")
                && merged.contains("let b = 2")
                && merged.contains("let c = 3"),
            "merged_text should include buffer + every sibling .crn. Got: {:?}",
            merged
        );
    }
}
