//! Configuration loading and .crn file discovery utilities

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::parser::{self, File, InferredFile, ParsedFile, ProviderContext};
use crate::schema::SchemaRegistry;
use crate::validation::inference::InferenceError;

/// Result of loading configuration, includes the file path containing backend block.
///
/// The struct holds non-blocking diagnostics (e.g. deferred for-iterable
/// binding errors) so the caller can decide to keep running the rest of
/// the static-analysis pipeline and report them together with downstream
/// findings (#2102). Blocking errors (bad parse, missing modules) still
/// come back as `Err` from the loader.
pub struct LoadedConfig {
    /// Post-inference file: every export carries a bare `TypeExpr`
    /// (possibly the `TypeExpr::Unknown` sentinel for failed inference,
    /// in which case a matching entry appears in `inference_errors`).
    pub parsed: InferredFile,
    /// Resources before reference resolution, for unused binding detection.
    /// After `resolve_resource_refs`, intermediate `ResourceRef` values are resolved away,
    /// so this preserves the original references for accurate unused binding analysis.
    /// Stays as `ParsedFile` because nothing downstream of this field
    /// reads `export_params.type_expr`.
    pub unresolved_parsed: ParsedFile,
    pub backend_file: Option<PathBuf>,
    /// Identifier-scope errors surfaced by [`parser::check_identifier_scope`]
    /// on the merged directory parse: ResourceRef roots in resources /
    /// attribute / module / export values and unresolved for-expression
    /// iterables. Empty when every reference resolves against the
    /// directory-wide binding set. The loader does not short-circuit on
    /// these so that later validators can also run in a single pass.
    pub identifier_scope_errors: Vec<parser::ParseError>,
    /// Inference errors collected by `apply_inference` (#2360 stage 2):
    /// one entry per `exports { ... }` declaration whose type could not
    /// be statically resolved. The corresponding entry in `parsed.export_params`
    /// carries `TypeExpr::Unknown` so downstream consumers can keep
    /// looking the export up by name without spawning cascading
    /// "missing export" diagnostics.
    pub inference_errors: Vec<(String, InferenceError)>,
}

/// Load configuration from a directory containing .crn files
pub fn load_configuration(path: &Path) -> Result<LoadedConfig, String> {
    load_configuration_with_config(path, &ProviderContext::default(), &SchemaRegistry::new())
}

/// Load configuration from a directory containing .crn files.
///
/// File paths are rejected — pass a directory instead.
pub fn load_configuration_with_config(
    path: &Path,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<LoadedConfig, String> {
    if path.is_file() {
        Err(format!("expected directory, got file: {}", path.display()))
    } else if path.is_dir() {
        // Directory mode
        let files = find_crn_files_in_dir(path)?;
        if files.is_empty() {
            return Err(format!("No .crn files found in {}", path.display()));
        }

        // Read every file once and run the directory-aware parse so
        // each file's `ParseContext` is seeded with the binding-name
        // union from sibling `.crn`s. Without this, e.g. an
        // `arguments {}` in `main.crn` would be invisible to `${env}`
        // interpolations in sibling `role.crn` (#2815, #2817).
        let mut file_inputs: Vec<(PathBuf, String)> = Vec::with_capacity(files.len());
        for file in &files {
            let content = fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            file_inputs.push((file.clone(), content));
        }
        let parsed_files = match parse_directory_files(&file_inputs, config) {
            Ok(v) => v,
            Err(e) => return Err(format!("{}: {}", path.display(), e)),
        };

        let empty_parsed = ParsedFile::default;
        let mut merged = empty_parsed();
        let mut unresolved_merged = empty_parsed();
        let mut parse_errors: Vec<String> = Vec::new();
        let mut backend_file: Option<PathBuf> = None;

        for (file, mut parsed) in parsed_files {
            // Stamp the full source path onto warnings and deferred
            // for-expressions. Bare filenames are ambiguous when a
            // project spans multiple `.crn` files (and identically
            // named files in sibling directories), and they break
            // editor jump-to-location.
            let file_path = Some(file.display().to_string());
            for w in &mut parsed.warnings {
                w.file = file_path.clone();
            }
            for d in &mut parsed.deferred_for_expressions {
                d.file = file_path.clone();
            }

            let mut unresolved = parsed.clone();
            if let Err(e) = parser::resolve_resource_refs_with_config(&mut parsed, config) {
                parse_errors.push(format!("{}: {}", file.display(), e));
                continue;
            }

            // The legacy form unrolled both `merge_parsed_file` calls
            // because the match arm needed independent control over
            // backend collision. Now that the match is gone, both
            // merges can delegate to the same helper. Backend collision
            // for `merged` is checked explicitly below (mirroring the
            // legacy "multiple backend blocks defined" error); the
            // unresolved copy only ever stored a single backend, so we
            // strip it before merging to avoid accidentally overwriting
            // `unresolved_merged.backend`.
            unresolved.backend = None;
            merge_parsed_file(&mut unresolved_merged, unresolved);

            let backend = parsed.backend.take();
            merge_parsed_file(&mut merged, parsed);
            if let Some(backend) = backend {
                if merged.backend.is_some() {
                    parse_errors.push(format!(
                        "{}: multiple backend blocks defined",
                        file.display()
                    ));
                } else {
                    merged.backend = Some(backend);
                    backend_file = Some(file.clone());
                }
            }
        }

        if !parse_errors.is_empty() {
            return Err(parse_errors.join("\n"));
        }

        // Resolve cross-file forward references on the merged result.
        // Per-file resolve_resource_refs_with_config (line 78) only sees
        // bindings within each file; cross-file dot-notation strings in
        // export_params (e.g., "registry_prod.account_id") remain as
        // Value::Concrete(ConcreteValue::String). This second pass converts them to ResourceRef.
        if let Err(e) = parser::resolve_resource_refs_with_config(&mut merged, config) {
            return Err(e.to_string());
        }

        // `finalize_provider_configs` is intentionally NOT called here.
        // The merged result is still pre-module-expansion, so deferred
        // `default_tags = mod.tags` shapes cannot be resolved yet. The
        // CLI calls finalize after `module_resolver::resolve_modules_with_config`
        // (see `carina-cli/src/commands/mod.rs`); LSP follows the same
        // contract. See #2717.

        // Identifier-scope checks are accumulated rather than short-
        // circuited so `carina validate` can keep going and report every
        // static error in one pass (#2102, #2126, #2138).
        let mut identifier_scope_errors = parser::check_identifier_scope(&merged);
        identifier_scope_errors.extend(parser::check_provider_instance_routing(&merged));

        // Phase transition: post-resolve, run rhs-driven inference over
        // every `exports { ... }` declaration so downstream consumers
        // see a definitive `TypeExpr` per export (#2360 stage 2).
        let (inferred, inference_errors) =
            crate::validation::inference::apply_inference(merged, schemas);

        Ok(LoadedConfig {
            parsed: inferred,
            unresolved_parsed: unresolved_merged,
            backend_file,
            identifier_scope_errors,
            inference_errors,
        })
    } else {
        Err(format!("Path not found: {}", path.display()))
    }
}

/// Parse all `.crn` files in a directory and return a merged `ParsedFile`.
///
/// This is the canonical directory-scope parse: each file is parsed
/// individually, results are merged, and cross-file references are
/// resolved against the combined binding map. Both CLI and LSP should
/// use this to ensure consistent results.
///
/// Returns `None` for `export_params` values that are cross-file string
/// references (e.g. `"registry_prod.account_id"`) resolved to `ResourceRef`.
pub fn parse_directory(dir: &Path, config: &ProviderContext) -> Result<ParsedFile, String> {
    parse_directory_with_overrides(dir, config, &HashMap::new())
}

/// Like [`parse_directory`] but takes an `overrides` map from **file name**
/// (e.g. `"main.crn"`) to source text. Files present in the map are parsed
/// from the override text; the rest are read from disk.
///
/// LSP uses this to analyze the open document's in-memory buffer alongside
/// the on-disk siblings, so edits show diagnostics before the user saves.
pub fn parse_directory_with_overrides(
    dir: &Path,
    config: &ProviderContext,
    overrides: &HashMap<String, String>,
) -> Result<ParsedFile, String> {
    let files = find_crn_files_in_dir(dir)?;
    let mut paths: Vec<(std::path::PathBuf, String)> = files
        .into_iter()
        .filter_map(|p| {
            p.file_name()
                .and_then(|n| n.to_str().map(|s| (p.clone(), s.to_string())))
        })
        .collect();
    // Also include overrides whose filename isn't on disk yet (e.g. a new
    // buffer the user hasn't saved). Keep the list de-duplicated by name.
    for name in overrides.keys() {
        if !paths.iter().any(|(_, n)| n == name) {
            paths.push((dir.join(name), name.clone()));
        }
    }
    if paths.is_empty() {
        return Err(format!("No .crn files found in {}", dir.display()));
    }

    // Resolve every file's source text (disk or override) up front so
    // the directory-aware parse helper sees the right inputs. #2817.
    let mut file_inputs: Vec<(PathBuf, String)> = Vec::with_capacity(paths.len());
    for (file, name) in &paths {
        let content = match overrides.get(name) {
            Some(buffer) => buffer.clone(),
            None => fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?,
        };
        file_inputs.push((file.clone(), content));
    }

    let parsed_files = match parse_directory_files(&file_inputs, config) {
        Ok(v) => v,
        Err(e) => return Err(format!("{}: {}", dir.display(), e)),
    };

    let mut merged = ParsedFile::default();

    for (file, mut parsed) in parsed_files {
        let file_path = Some(file.display().to_string());
        for w in &mut parsed.warnings {
            w.file = file_path.clone();
        }
        for d in &mut parsed.deferred_for_expressions {
            d.file = file_path.clone();
        }
        merge_parsed_file(&mut merged, parsed);
    }

    // Resolve cross-file references on the merged result
    if let Err(e) = parser::resolve_resource_refs_with_config(&mut merged, config) {
        return Err(e.to_string());
    }

    // `finalize_provider_configs` is intentionally NOT called here. The
    // merged result is pre-module-expansion; deferred provider
    // attributes that reference module-call bindings cannot be resolved
    // until `module_resolver::resolve_modules_with_config` runs.
    // Consumers (CLI, LSP) call finalize themselves after expansion.
    // See #2717.

    // Identifier-scope validation is left to callers. LSP needs the
    // ParsedFile even when a reference is unresolved, so it can surface
    // the error as a diagnostic; CLI entry points call
    // `parser::check_identifier_scope` themselves (see
    // `load_configuration_with_config`).
    Ok(merged)
}

/// Re-label a parsed file's export-param phase (`File<A>` → `File<B>`)
/// when it carries **no** export params.
///
/// Module expansion produces a concrete `ParsedFile` contribution (it
/// is a parser-phase operation), but the caller's target is a generic
/// `File<E>` (`resolve_modules_with_config<E>`). Today every
/// production caller instantiates `E = ParsedExportParam`, so this is
/// a same-phase no-op; the `<A, B>` generality keeps
/// `resolve_modules_with_config` phase-agnostic for a future
/// inferred-phase caller without re-introducing a hand-listed field
/// list. A module contribution never re-exports raw export params —
/// `export_params` is always empty — so the relabel is total and
/// lossless.
///
/// Delegates to [`File::map_export_params`](crate::parser::File), the
/// single exhaustive-destructure phase-axis surface (the carina#3126 /
/// carina#3061 compile-time forcing function: a new `File<E>` field
/// stops `map_export_params` from compiling until classified, so the
/// module-expansion bridge can never silently drop a field).
/// `debug_assert!`s `export_params` is empty — a contract tripwire for
/// a future caller that violates the precondition; not reachable from
/// the sole current caller (`expand_module_call` pins
/// `export_params: Vec::new()`).
pub(crate) fn relabel_export_phase<A, B>(f: File<A>) -> File<B> {
    f.map_export_params(|export_params| {
        // Tripwire for a future caller that violates the precondition.
        // `debug_assert!` (not `assert!`): carina-core is a library;
        // the real enforcement is the exhaustive destructure in
        // `map_export_params`, and the sole current caller pins
        // `export_params: Vec::new()`, so this is unreachable today —
        // a release-build crash here would be a strictly worse failure
        // mode than the test/CI catch a debug assert already gives.
        debug_assert!(
            export_params.is_empty(),
            "relabel_export_phase: only an export-param-free file may cross \
             phases; a module contribution must never carry export_params"
        );
        Vec::new()
    })
}

/// Fold one parsed file's content into another (the sibling-`.crn`
/// directory merge, and the module-load merge).
///
/// `source` is **destructured exhaustively** rather than field-accessed:
/// this is the single source of truth for "every mergeable `File<E>`
/// field", and the destructure is a compile-time forcing function — if
/// a field is added to `File<E>`, this stops compiling until someone
/// decides how it merges. Without that guard a new field is silently
/// dropped on this path, which is exactly the carina#3126 / carina#3061
/// class of bug (a `File<E>` field that one merge path forgot). Generic
/// over the export-param phase `E` so the parser phase and inferred
/// phase share one merge.
pub(crate) fn merge_parsed_file<E>(target: &mut File<E>, source: File<E>) {
    let File {
        providers,
        resources,
        data_sources,
        compositions,
        variables,
        uses,
        module_calls,
        arguments,
        attribute_params,
        export_params,
        backend,
        state_blocks,
        user_functions,
        upstream_states,
        wait_bindings,
        requires,
        structural_bindings,
        warnings,
        deferred_for_expressions,
        expansion_trace,
    } = source;

    target.providers.extend(providers);
    target.resources.extend(resources);
    target.data_sources.extend(data_sources);
    target.compositions.extend(compositions);
    target.variables.extend(variables);
    target.uses.extend(uses);
    target.module_calls.extend(module_calls);
    target.arguments.extend(arguments);
    target.attribute_params.extend(attribute_params);
    target.export_params.extend(export_params);
    target.state_blocks.extend(state_blocks);
    target.user_functions.extend(user_functions);
    target.upstream_states.extend(upstream_states);
    target.wait_bindings.extend(wait_bindings);
    target.requires.extend(requires);
    target.structural_bindings.extend(structural_bindings);
    target.warnings.extend(warnings);
    target
        .deferred_for_expressions
        .extend(deferred_for_expressions);
    // `expansion_trace`: merge every (leaf, chain) entry. Different
    // sources never produce the same leaf id (the expander prefixes
    // leaf ids with the call-site instance), so the merge is just a
    // disjoint-set union; the iteration order does not matter.
    for (leaf, call_sites) in expansion_trace.leaf_to_call_sites {
        target.expansion_trace.record(leaf, call_sites);
    }
    // `backend` is config, not accumulated content: last file wins.
    if let Some(backend) = backend {
        target.backend = Some(backend);
    }
}

/// Parse every `(path, source)` pair as part of a single directory unit.
///
/// Two-pass implementation that addresses the sibling-scope class
/// (#2817):
///
/// - **Pass 1** parses every input with an empty seed list. This is the
///   legacy per-file parse — sibling-defined names are not in scope, so
///   ResourceRef / String fallback paths fire as before. The Pass-1
///   `ParsedFile`s are merged only to collect the *binding-name union*
///   from sibling files; the merged result itself is discarded.
/// - **Pass 2** parses every input again, this time seeding the per-file
///   `ParseContext` with the binding-name union from Pass 1. Names that
///   originate in *sibling* files now resolve through the normal
///   `ctx.get_variable` / `ctx.is_resource_binding` paths, so an
///   `arguments {}` block in `main.crn` is visible from `role.crn`,
///   a `let` declared in `helpers.crn` is visible from `main.crn`,
///   etc.
///
/// The returned vector preserves the input order. Any per-file parse
/// error from Pass 2 short-circuits and is returned to the caller; Pass
/// 1 swallows nothing — if Pass 1 fails on a file, Pass 2 will fail on
/// the same file with the same error.
///
/// Pass-1 cost is paid once per directory load. Each call to
/// `parse_with_seeded_bindings` with an empty seed list is a no-op
/// in `seed_bindings`, so single-file callers (parser tests, the
/// `parse(input, config)` wrapper) pay zero overhead.
pub fn parse_directory_files(
    files: &[(PathBuf, String)],
    config: &ProviderContext,
) -> Result<Vec<(PathBuf, ParsedFile)>, parser::ParseError> {
    // Pass 1: collect the binding-name union from a regular per-file
    // parse. Errors here propagate identically to Pass 2 (same input,
    // same parser path), so don't try to be clever about saving them.
    let mut union = ParsedFile::default();
    for (_, content) in files {
        let parsed = parser::parse(content, config)?;
        merge_parsed_file(&mut union, parsed);
    }
    let seeds: Vec<&str> = parser::collect_known_bindings_merged(&union)
        .into_iter()
        .collect();

    // Pass 2: re-parse each file with the union seeded into `ctx`.
    let mut out = Vec::with_capacity(files.len());
    for (path, content) in files {
        let parsed = parser::parse_with_seeded_bindings(content, config, &seeds)?;
        out.push((path.clone(), parsed));
    }
    Ok(out)
}

/// Get base directory for module resolution.
///
/// Since paths are now always directories, this returns the path as-is.
/// Kept for backward compatibility with callers.
pub fn get_base_dir(path: &Path) -> &Path {
    path
}

/// Find all .crn files recursively in a directory, skipping hidden dirs,
/// target, and node_modules.
///
/// The returned paths are sorted lexicographically by `PathBuf` ordering so
/// callers get deterministic results across filesystems (#2851). This matches
/// the contract of [`find_crn_files_in_dir`].
pub fn find_crn_files_recursive(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_crn_files_recursive(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_crn_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    // Sort directory entries before descending so traversal order is
    // independent of the underlying filesystem's `readdir` order.
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();

    for path in paths {
        if path.is_dir() {
            // Skip hidden directories and common non-source directories
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_crn_files_recursive(&path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }

    Ok(())
}

/// Find .crn files in a single directory (non-recursive).
///
/// The returned paths are sorted by `PathBuf` ordering, which for a
/// single-directory listing degenerates to filename order. Callers that
/// walk siblings independently (rather than going through
/// `parse_directory`'s merge path) get deterministic order across
/// filesystems (#2449).
pub fn find_crn_files_in_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper to create a temporary directory for tests
    fn create_temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("carina_config_loader_test_{}", name));
        // Clean up if it exists from a previous run
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Helper to clean up a temporary directory
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    // ========== find_crn_files_in_dir tests ==========

    #[test]
    fn find_crn_files_in_dir_empty_directory() {
        let dir = create_temp_dir("in_dir_empty");
        let result = find_crn_files_in_dir(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_with_crn_files() {
        let dir = create_temp_dir("in_dir_with_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.crn"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].ends_with("a.crn"));
        assert!(result[1].ends_with("b.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_returns_paths_sorted_by_name() {
        // #2449: callers that walk siblings independently (rather than
        // through `parse_directory`'s merge path) used to inherit the
        // filesystem-dependent order. Pin lexicographic order so first-
        // match-wins lookups are deterministic across ext4 / APFS / tmpfs.
        // Insert the files in reverse name order to defeat the common
        // "creation order == listing order" fallback on tmpfs.
        let dir = create_temp_dir("in_dir_sorted");
        for name in ["z.crn", "m.crn", "a.crn", "providers.crn", "main.crn"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let result = find_crn_files_in_dir(&dir).unwrap();
        let names: Vec<String> = result
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a.crn", "m.crn", "main.crn", "providers.crn", "z.crn"]
        );
        cleanup(&dir);
    }

    // #2465: pin the signature as `&Path`-accepting at compile time, so a
    // future revert to `&PathBuf` fails to build instead of silently
    // re-introducing the `.to_path_buf()` ceremony at every LSP callsite.
    const _: fn(&Path) -> Result<Vec<PathBuf>, String> = find_crn_files_in_dir;

    #[test]
    fn find_crn_files_in_dir_ignores_non_crn_files() {
        let dir = create_temp_dir("in_dir_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.rs"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_does_not_recurse() {
        let dir = create_temp_dir("in_dir_no_recurse");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();

        let result = find_crn_files_in_dir(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("top.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_in_dir_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_dir_xyz");
        let result = find_crn_files_in_dir(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    // ========== find_crn_files_recursive tests ==========

    #[test]
    fn find_crn_files_recursive_empty_directory() {
        let dir = create_temp_dir("recursive_empty");
        let result = find_crn_files_recursive(&dir).unwrap();
        assert!(result.is_empty());
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_finds_nested_files() {
        let dir = create_temp_dir("recursive_nested");
        fs::write(dir.join("top.crn"), "").unwrap();
        let sub = dir.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("nested.crn"), "").unwrap();
        let deep = sub.join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.crn"), "").unwrap();

        let mut result = find_crn_files_recursive(&dir).unwrap();
        result.sort();
        assert_eq!(result.len(), 3);
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_hidden_directories() {
        let dir = create_temp_dir("recursive_hidden");
        fs::write(dir.join("visible.crn"), "").unwrap();
        let hidden = dir.join(".hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("secret.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("visible.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_target_directory() {
        let dir = create_temp_dir("recursive_target");
        fs::write(dir.join("main.crn"), "").unwrap();
        let target = dir.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("build.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("main.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_skips_node_modules() {
        let dir = create_temp_dir("recursive_node_modules");
        fs::write(dir.join("app.crn"), "").unwrap();
        let nm = dir.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("dep.crn"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("app.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_ignores_non_crn_files() {
        let dir = create_temp_dir("recursive_non_crn");
        fs::write(dir.join("a.crn"), "").unwrap();
        fs::write(dir.join("b.txt"), "").unwrap();
        fs::write(dir.join("c.json"), "").unwrap();

        let result = find_crn_files_recursive(&dir).unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].ends_with("a.crn"));
        cleanup(&dir);
    }

    #[test]
    fn find_crn_files_recursive_nonexistent_directory() {
        let dir = PathBuf::from("/tmp/carina_config_loader_test_recursive_nonexistent_xyz");
        let result = find_crn_files_recursive(&dir);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to read directory"));
    }

    /// Regression test for #2851: recursive .crn discovery must return
    /// paths in lexicographic order regardless of the underlying
    /// filesystem's `readdir` order. We create the files in
    /// reverse-lexicographic order to make non-deterministic ordering
    /// more likely to surface on filesystems that return entries in
    /// creation order.
    #[test]
    fn find_crn_files_recursive_returns_paths_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create sibling files and nested directories in reverse order.
        let sub_z = root.join("z_sub");
        let sub_a = root.join("a_sub");
        fs::create_dir_all(&sub_z).unwrap();
        fs::create_dir_all(&sub_a).unwrap();

        fs::write(sub_z.join("z.crn"), "").unwrap();
        fs::write(sub_z.join("a.crn"), "").unwrap();
        fs::write(sub_a.join("z.crn"), "").unwrap();
        fs::write(sub_a.join("a.crn"), "").unwrap();
        fs::write(root.join("z_top.crn"), "").unwrap();
        fs::write(root.join("a_top.crn"), "").unwrap();

        let result = find_crn_files_recursive(root).unwrap();

        let mut expected = result.clone();
        expected.sort();
        assert_eq!(
            result, expected,
            "find_crn_files_recursive must return paths in lexicographic order"
        );
        assert_eq!(result.len(), 6);
    }

    // ========== get_base_dir tests ==========

    #[test]
    fn get_base_dir_returns_path_as_is() {
        let dir = create_temp_dir("base_dir_as_is");

        let base = get_base_dir(&dir);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_directory() {
        let dir = create_temp_dir("base_dir_directory");

        let base = get_base_dir(&dir);
        assert_eq!(base, dir.as_path());
        cleanup(&dir);
    }

    #[test]
    fn get_base_dir_for_nonexistent_path() {
        // Non-existent path is neither file nor dir, so returns the path itself
        let path = Path::new("/tmp/carina_nonexistent_path_xyz");
        let base = get_base_dir(path);
        assert_eq!(base, path);
    }

    // ========== load_configuration tests ==========

    #[test]
    fn load_configuration_runs_inference_and_surfaces_errors() {
        // Stage 2 (#2360): an unannotated dynamic-rhs export
        // (`lookup` returns Any) becomes a sentinel + an inference
        // error rather than slipping through unchecked.
        let dir = create_temp_dir("load_inference_failure");
        fs::write(
            dir.join("main.crn"),
            "exports {\n  zone_id = lookup({a = \"1\"}, \"a\", \"default\")\n}\n",
        )
        .unwrap();

        let loaded = load_configuration_with_config(
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .unwrap();
        cleanup(&dir);
        assert_eq!(loaded.parsed.export_params.len(), 1);
        assert_eq!(
            loaded.parsed.export_params[0].type_expr,
            crate::parser::TypeExpr::Unknown,
        );
        assert_eq!(loaded.inference_errors.len(), 1);
    }

    #[test]
    fn load_configuration_keeps_inferable_export_typed() {
        // String-literal rhs is statically inferable; no annotation
        // required, no inference error.
        let dir = create_temp_dir("load_inferable_literal");
        fs::write(dir.join("main.crn"), "exports {\n  name = \"carina\"\n}\n").unwrap();

        let loaded = load_configuration_with_config(
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .unwrap();
        cleanup(&dir);
        assert!(
            loaded.inference_errors.is_empty(),
            "no inference errors expected, got {:?}",
            loaded.inference_errors,
        );
        assert_eq!(
            loaded.parsed.export_params[0].type_expr,
            crate::parser::TypeExpr::String,
        );
    }

    #[test]
    fn load_configuration_directory_with_provider() {
        let dir = create_temp_dir("load_dir_provider");
        fs::write(
            dir.join("test.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_backend() {
        let dir = create_temp_dir("load_dir_backend");
        fs::write(
            dir.join("test.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_nonexistent_path() {
        let path = PathBuf::from("/tmp/carina_config_loader_test_nonexistent_file_xyz");
        let result = load_configuration(&path);
        match result {
            Err(e) => assert!(e.contains("Path not found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for nonexistent path"),
        }
    }

    #[test]
    fn load_configuration_empty_directory() {
        let dir = create_temp_dir("load_empty_dir");
        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(e.contains("No .crn files found"), "unexpected error: {}", e),
            Ok(_) => panic!("expected error for empty directory"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_single_file() {
        let dir = create_temp_dir("load_dir_single");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.backend_file.is_none());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_merges_multiple_files() {
        let dir = create_temp_dir("load_dir_merge");
        fs::write(
            dir.join("provider.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("backend.crn"),
            r#"backend s3 {
    bucket = "my-bucket"
    key    = "state.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        assert!(config.parsed.backend.is_some());
        assert!(config.backend_file.is_some());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_ignores_non_crn_files() {
        let dir = create_temp_dir("load_dir_ignore_non_crn");
        fs::write(
            dir.join("main.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();
        fs::write(dir.join("notes.txt"), "not a crn file").unwrap();

        let config = load_configuration(&dir).unwrap();
        assert_eq!(config.parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_with_parse_error() {
        let dir = create_temp_dir("load_dir_parse_error");
        fs::write(dir.join("bad.crn"), "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&dir);
        assert!(result.is_err());
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_rejects_file_path() {
        let dir = create_temp_dir("load_rejects_file");
        let file = dir.join("test.crn");
        fs::write(
            &file,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_file_path_rejected_even_with_bad_content() {
        let dir = create_temp_dir("load_file_parse_error");
        let file = dir.join("bad.crn");
        fs::write(&file, "this is not valid crn syntax {{{").unwrap();

        let result = load_configuration(&file);
        match result {
            Err(e) => assert!(
                e.contains("expected directory, got file"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for file path"),
        }
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_stamps_full_path_on_warnings() {
        // Issue #1997: warnings and deferred for-expressions must carry a full
        // source path, not a bare filename. Bare filenames are ambiguous when a
        // project spans multiple .crn files and break editor jump-to-location.
        let dir = create_temp_dir("load_warning_full_path");
        // A for-loop with an unused binding generates a ParseWarning.
        let crn_path = dir.join("main.crn");
        fs::write(
            &crn_path,
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}

let empty_list = []
let _ = for unused_var in empty_list {
    aws.ec2.Vpc {
        cidr_block = "10.0.0.0/16"
    }
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        assert!(
            !config.parsed.warnings.is_empty(),
            "fixture must produce at least one ParseWarning"
        );
        let stamped = config.parsed.warnings[0]
            .file
            .as_ref()
            .expect("warning must have a file path stamped");
        assert_eq!(
            stamped,
            &crn_path.display().to_string(),
            "warning.file must carry the full source path, not a bare filename",
        );
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_preserves_unresolved_parsed() {
        let dir = create_temp_dir("load_unresolved");
        fs::write(
            dir.join("test.crn"),
            r#"provider aws {
    region = aws.Region.ap_northeast_1
}
"#,
        )
        .unwrap();

        let config = load_configuration(&dir).unwrap();
        // unresolved_parsed should also have the provider
        assert_eq!(config.unresolved_parsed.providers.len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn load_configuration_directory_multiple_backends_error() {
        let dir = create_temp_dir("load_dir_multi_backend");
        fs::write(
            dir.join("a.crn"),
            r#"backend s3 {
    bucket = "bucket-a"
    key    = "state-a.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("b.crn"),
            r#"backend s3 {
    bucket = "bucket-b"
    key    = "state-b.json"
    region = "ap-northeast-1"
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        match result {
            Err(e) => assert!(
                e.contains("multiple backend blocks defined"),
                "unexpected error: {}",
                e
            ),
            Ok(_) => panic!("expected error for multiple backends"),
        }
        cleanup(&dir);
    }

    #[test]
    fn parse_directory_accepts_cross_file_upstream_state_in_for_expression() {
        let dir = create_temp_dir("cross_file_upstream_for");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.Bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        assert!(
            result.is_ok(),
            "expected cross-file upstream_state binding in `for` to resolve, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn cross_file_upstream_state_refs_emit_no_soft_warning() {
        // Field validity is checked statically by the `upstream_exports`
        // module now. Loader- and parser-level "validate does not inspect"
        // soft warnings are gone across the board.
        let dir = create_temp_dir("cross_file_upstream_no_warning");
        fs::write(
            dir.join("backend.crn"),
            r#"backend local { path = 'carina.state.json' }

let orgs = upstream_state {
  source = '../organizations'
}

let network = upstream_state {
  source = '../network'
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in orgs.accounts {
  aws.s3.Bucket {
    name = name
  }
}

awscc.ec2.SecurityGroup {
  group_description = 'Web SG'
  vpc_id = network.vpc_id
}
"#,
        )
        .unwrap();

        let result = load_configuration(&dir);
        let loaded = result.expect("load should succeed");
        cleanup(&dir);

        let upstream_warnings: Vec<_> = loaded
            .parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("upstream_state"))
            .collect();
        assert!(
            upstream_warnings.is_empty(),
            "loader should emit no soft upstream_state warnings, got: {:?}",
            upstream_warnings
        );
    }

    #[test]
    fn parse_directory_leaves_undefined_iterable_for_caller_to_check() {
        let dir = create_temp_dir("undefined_for_iterable");
        fs::write(
            dir.join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  aws.s3.Bucket {
    name = name
  }
}
"#,
        )
        .unwrap();

        let result = parse_directory(&dir, &ProviderContext::default());
        cleanup(&dir);
        let parsed = result.expect("parse_directory should not fail on undefined iterable");
        let errs = parser::check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        match &errs[0] {
            parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected error: {other}"),
        }

        // `load_configuration_with_config` surfaces the error via
        // `LoadedConfig::identifier_scope_errors` rather than short-
        // circuiting, so the CLI caller can collect it together with
        // findings from later validators in a single pass (#2102).
        let dir2 = create_temp_dir("undefined_for_iterable_cli");
        fs::write(
            dir2.join("main.crn"),
            r#"for name, account_id in does_not_exist.accounts {
  aws.s3.Bucket {
    name = name
  }
}
"#,
        )
        .unwrap();
        let loaded = load_configuration_with_config(
            &dir2,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .expect("load_configuration should not short-circuit on identifier-scope errors");
        cleanup(&dir2);
        assert_eq!(
            loaded.identifier_scope_errors.len(),
            1,
            "expected one identifier-scope error, got {:?}",
            loaded.identifier_scope_errors,
        );
        match &loaded.identifier_scope_errors[0] {
            parser::ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "does_not_exist");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    // Acceptance for #2126. Per-file parse must not reject a ResourceRef
    // whose root is declared in a sibling `.crn`. The check lives on the
    // merged `ParsedFile` via `check_identifier_scope` (#2138).
    #[test]
    fn parse_directory_accepts_cross_file_resource_ref() {
        let dir = create_temp_dir("cross_file_resource_ref");
        fs::write(
            dir.join("main.crn"),
            r#"let attach = aws.organizations.attach {
    target_id = caller.account_id
}
"#,
        )
        .unwrap();
        fs::write(
            dir.join("backend.crn"),
            r#"let caller = read aws.sts.caller_identity {}
"#,
        )
        .unwrap();

        let loaded = load_configuration_with_config(
            &dir,
            &ProviderContext::default(),
            &SchemaRegistry::new(),
        )
        .expect("directory with cross-file ResourceRef must load without error");
        cleanup(&dir);
        assert!(
            loaded.identifier_scope_errors.is_empty(),
            "no identifier-scope errors expected, got: {:?}",
            loaded.identifier_scope_errors,
        );
    }
}
