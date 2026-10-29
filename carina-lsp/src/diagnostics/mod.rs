mod checks;
mod validation;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::Document;
use crate::position;
use carina_core::parser::{
    ParseError, ParsedFile, ResourceRef, SourceSpan, WarningKind, contains_function_call,
};
use carina_core::provider::{
    HostErrorCause, ProviderError, ProviderFactory, SuspendedValidationId,
};
use carina_core::resource::{ConcreteValue, DeferredValue, Value};
use carina_core::schema::{SchemaRegistry, TypeError, TypeInSchema};

fn parser_span_range(doc: &Document, span: SourceSpan) -> Option<Range> {
    let text = doc.text();
    let position = |byte: usize| {
        if byte > text.len() || !text.is_char_boundary(byte) {
            return None;
        }
        let prefix = &text[..byte];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = prefix.rfind('\n').map_or(0, |index| index + 1);
        Some(Position {
            line,
            character: position::byte_offset_to_char_offset(
                &text[line_start..byte],
                byte - line_start,
            ),
        })
    };
    Some(Range {
        start: position(span.start_byte())?,
        end: position(span.end_byte())?,
    })
}

#[derive(Default)]
pub(super) struct ProviderValidationDeduplicator {
    suspended_inputs: Mutex<HashSet<(String, SuspendedValidationId)>>,
}

impl ProviderValidationDeduplicator {
    fn should_emit(&self, provider: &str, error: &ProviderError) -> bool {
        let Some(HostErrorCause::ValidationSuspended(input_id)) = error.detail().host_cause()
        else {
            return true;
        };
        self.suspended_inputs
            .lock()
            .expect("provider validation diagnostic lock must not be poisoned")
            .insert((provider.to_string(), input_id))
    }

    fn should_emit_type_error(&self, error: &TypeError) -> bool {
        let Some(HostErrorCause::ValidationSuspended(input_id)) = error.host_cause() else {
            return true;
        };
        self.suspended_inputs
            .lock()
            .expect("provider validation diagnostic lock must not be poisoned")
            .insert((
                error.provider_name().unwrap_or("<unknown>").to_string(),
                input_id,
            ))
    }
}

/// Create a `Diagnostic` on a single line with the standard "carina" source.
pub(crate) fn carina_diagnostic(
    line: u32,
    start_col: u32,
    end_col: u32,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: start_col,
            },
            end: Position {
                line,
                character: end_col,
            },
        },
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

fn deduplicate_identical_diagnostics(diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = HashSet::new();
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            seen.insert((
                diagnostic.range.start.line,
                diagnostic.range.start.character,
                diagnostic.range.end.line,
                diagnostic.range.end.character,
                format!("{:?}", diagnostic.severity),
                diagnostic.message.clone(),
            ))
        })
        .collect()
}

/// Create a `Diagnostic` with an arbitrary `Range` and the standard "carina" source.
pub(crate) fn carina_diagnostic_range(
    range: Range,
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    }
}

pub struct DiagnosticEngine {
    schemas: Arc<SchemaRegistry>,
    provider_names: Vec<String>,
    known_provider_names: HashSet<String>,
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    /// Providers that failed to load: name -> error reason.
    provider_errors: HashMap<String, String>,
    /// Cached provider context with custom type validators from schemas.
    provider_context: carina_core::parser::ProviderContext,
}

impl DiagnosticEngine {
    pub fn new(
        schemas: Arc<SchemaRegistry>,
        provider_names: Vec<String>,
        factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> Self {
        Self::new_with_known_providers(schemas, provider_names, factories, HashSet::new())
    }

    pub(crate) fn new_with_known_providers(
        schemas: Arc<SchemaRegistry>,
        provider_names: Vec<String>,
        factories: Arc<Vec<Box<dyn ProviderFactory>>>,
        known_provider_names: HashSet<String>,
    ) -> Self {
        // carina#3239 gate: the strict "unknown custom type in type
        // position" parser check requires the project to actually
        // *have* a provider surface to compare against. The LSP
        // backend keeps an *empty* fallback `ProviderState` for files
        // that don't belong to any known config directory and aren't
        // reachable through the import map (`carina-lsp/src/backend.rs`'s
        // `ProviderStates::empty`); for that fallback, no provider
        // was ever declared and the orphaned file has no way to
        // resolve a custom type name, so disabling the strict check
        // avoids a flood of false positives.
        //
        // The flag keys off `provider_names` rather than
        // `factories.is_empty()` so a project whose declared
        // providers failed to load (network error, missing install,
        // …) still gets the strict check — the user sees the provider
        // load error and the unknown-custom-type error together
        // instead of having the carina#3239 fix silently masked by an
        // unrelated install failure.
        let has_loaded_providers = !provider_names.is_empty();
        let custom_types = if has_loaded_providers {
            carina_core::provider::provider_custom_types_from_factories(Arc::clone(&factories))
        } else {
            carina_core::parser::ProviderCustomTypes::default()
        };
        Self::new_with_known_providers_and_custom_types(
            schemas,
            provider_names,
            factories,
            known_provider_names,
            custom_types,
        )
    }

    pub(crate) fn new_with_known_providers_and_custom_types(
        schemas: Arc<SchemaRegistry>,
        provider_names: Vec<String>,
        factories: Arc<Vec<Box<dyn ProviderFactory>>>,
        known_provider_names: HashSet<String>,
        custom_types: carina_core::parser::ProviderCustomTypes,
    ) -> Self {
        let provider_context = carina_core::parser::ProviderContext {
            decryptor: None,
            custom_types,
            resource_types:
                carina_core::parser::ProviderContext::resource_types_from_schema_registry(&schemas),
        };
        Self {
            schemas,
            provider_names,
            known_provider_names,
            factories,
            provider_errors: HashMap::new(),
            provider_context,
        }
    }

    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }

    pub fn with_provider_errors(mut self, errors: HashMap<String, String>) -> Self {
        self.provider_errors = errors;
        self
    }

    /// Analyze an open document in its directory context.
    ///
    /// `current_file_path` is the document's full path. It is the sole source
    /// of file identity; the directory and basename used for merged parsing
    /// are derived from it rather than inferred from buffer contents.
    pub fn analyze(&self, doc: &Document, current_file_path: Option<&Path>) -> Vec<Diagnostic> {
        #[cfg(test)]
        if let Some(current_file_path) = current_file_path {
            assert_eq!(
                doc.source_file(),
                current_file_path,
                "document source path must match the analyzed path",
            );
        }
        let mut diagnostics = Vec::new();
        let provider_validation_deduplicator = ProviderValidationDeduplicator::default();
        let text = doc.text();
        let base_path = current_file_path.map(|path| {
            path.parent()
                .expect("a current file path must have a parent directory")
        });
        let current_file_name = current_file_path.map(|path| {
            path.file_name()
                .expect("a current file path must name a file")
                .to_string_lossy()
        });
        let current_file_name = current_file_name.as_deref();

        // Empty `${}` interpolation hint. Runs even when other parses
        // succeeded — and especially before the buffer-only checks
        // below — so users get an actionable warning right at the
        // placeholder rather than only learning at apply time.
        diagnostics.extend(self.check_empty_interpolations(doc));

        // Build the directory-scoped merged parse once up-front — it is
        // the authoritative source of truth for every check that needs
        // cross-file context: undefined-identifier (#2132), upstream-
        // state field references, for-iterable bindings, unused-let
        // filtering, and exports type validation (#2134). The buffer is
        // substituted for its on-disk copy so diagnostics update on
        // keystrokes.
        let merged_attempt =
            base_path.map(|base| self.parse_merged_with_buffer(doc, current_file_name, base));
        let directory_error_emitted = if let (Some(Err(error)), Some(current_file_path)) =
            (merged_attempt.as_ref(), current_file_path)
            && error
                .source_file()
                .is_some_and(|source_file| source_file == current_file_path)
            && let Some(parse_error) = error.parse_error()
        {
            // The directory parse has the classified sibling bindings and is
            // therefore more authoritative than the buffer-only parse (which
            // may have treated a sibling name as an enum identifier).
            diagnostics.push(parse_error_to_diagnostic(parse_error));
            true
        } else {
            false
        };
        let merged_result = merged_attempt.and_then(Result::ok);
        // A buffer parsed by itself cannot resolve sibling user functions.
        // When the authoritative directory parse succeeds, use its per-file
        // semantic AST for source-oriented checks instead of dropping every
        // check merely because the provisional document parse failed.
        let merged_current_file = || {
            current_file_path.and_then(|path| {
                merged_result
                    .as_ref()
                    .and_then(|result| result.directory.semantic_file(path))
            })
        };
        let current_parsed = doc.parsed().or_else(merged_current_file);
        // A source parsed in isolation cannot know sibling function
        // declarations. Prefer the successful directory parse over that
        // provisional failure (notably a sibling user function used as a
        // `for` iterable). When the directory parse also fails—or there is no
        // directory context—the document-owned parse error remains
        // authoritative.
        if merged_result.is_none()
            && !directory_error_emitted
            && let Some(error) = doc.parse_error()
        {
            diagnostics.push(parse_error_to_diagnostic(error));
        }
        // The diagnostic directory parse deliberately preserves a user-function
        // call when evaluating its body fails, so the rest of the editor checks
        // can still inspect the authored call. Run the strict resolver on a
        // clone solely to surface source-owned body errors (for example, a
        // sibling call that binds a structural handle and then uses the
        // parameter bare). `ParseError::Located` keeps the diagnostic on the
        // function document rather than the caller document.
        if let (Some(result), Some(current_file_path)) = (merged_result.as_ref(), current_file_path)
        {
            let mut strict = result.directory.parsed.clone();
            if let Err(error) = carina_core::parser::resolve_resource_refs_with_config(
                &mut strict,
                &self.provider_context,
            ) && error
                .source_file()
                .is_some_and(|source_file| source_file == current_file_path)
            {
                diagnostics.push(parse_error_to_diagnostic(&error));
            }
        }
        if let Some(result) = merged_result.as_ref() {
            diagnostics.extend(self.duplicate_declaration_diagnostics(
                doc,
                current_file_name,
                &result.directory.duplicate_declarations,
            ));
        } else if let Some(parsed) = current_parsed {
            // An unrelated broken sibling makes the directory parse fail, but
            // the current buffer's successful parse still retains same-file
            // declaration multiplicity. Reuse the loader extractor so export,
            // argument, and module-attribute duplicates remain visible without
            // recreating directory provenance that is unavailable in this mode.
            let duplicates = carina_core::config_loader::find_duplicate_declarations_in_parsed_file(
                current_file_path.unwrap_or_else(|| Path::new("<buffer>.crn")),
                parsed,
            );
            diagnostics.extend(self.duplicate_declaration_diagnostics(
                doc,
                current_file_name,
                &duplicates,
            ));
        }
        if let (Some(base), Some(current_file), Some(result)) =
            (base_path, current_file_name, merged_result.as_ref())
        {
            diagnostics.extend(self.check_mixed_tag_key_styles(
                current_file,
                base,
                &result.directory,
            ));
        }
        let merged = merged_result
            .as_ref()
            .map(|result| &result.directory.parsed);
        if let Some(current_file) = current_parsed {
            diagnostics.extend(self.attribute_derived_anonymous_resource_diagnostics(
                doc,
                current_file,
                merged.unwrap_or(current_file),
            ));
        }
        let upstream_resolution = match (base_path, merged) {
            (Some(base), Some(merged)) => Some(
                carina_core::upstream_exports::resolve_upstream_exports_with_schemas(
                    base,
                    &merged.upstream_states,
                    &self.provider_context,
                    Some(&self.schemas),
                ),
            ),
            _ => None,
        };

        if let (Some(merged), Some((exports, resolve_errors))) =
            (merged, upstream_resolution.as_ref())
        {
            diagnostics.extend(self.check_upstream_state_field_references(
                doc,
                merged,
                exports,
                resolve_errors,
            ));
            diagnostics.extend(self.check_for_iterable_bindings(doc, merged, current_file_name));
        }

        // carina#2191 Phase 5: surface `check_provider_instance_routing`
        // errors as editor diagnostics. The CLI's `load_configuration`
        // accumulates them in `identifier_scope_errors`; LSP runs the
        // same pass against the merged parse so users see the same
        // "unknown provider instance" / "no default instance for kind"
        // message before they run `carina validate`.
        if let Some(merged) = merged {
            for err in carina_core::parser::check_provider_instance_routing(merged) {
                diagnostics.push(parse_error_to_diagnostic(&err));
            }
        }

        // carina#3239: surface unknown bare custom-type names in
        // `arguments` / `attributes` / `exports` declarations as
        // editor diagnostics. Two paths feed the walker:
        //
        // 1. The directory-merged parse (`merged`) — when present,
        //    it carries every sibling `.crn`'s declarations so an
        //    `arguments { foo: TotallyMadeUpType }` in
        //    `args.crn` is caught while editing `main.crn`.
        //
        // 2. The buffer-only parse (`doc.parsed()`) — load-bearing
        //    fallback when the merged parse failed. The merged parse
        //    uses the enriched `ProviderCustomTypes::Loaded` context, so the
        //    parser's strict gate raises `ParseError::InvalidExpression`
        //    on the very unknown-type-name shape we want to report.
        //    `parse_merged_with_buffer` returns that directory error; the
        //    owner-aware path above surfaces errors from the current file,
        //    while this fallback still handles shapes found only in the
        //    bootstrap AST. The buffer-only parse uses the bootstrap context
        //    (`ProviderCustomTypes::Unloaded`), so it always succeeds and
        //    `doc.parsed()` is `Some`, letting the walker still run
        //    against the user's in-flight edits.
        //
        // Both paths walk the same `arguments` set when the edited
        // file is the one declaring them, so the merged path is the
        // strict preference and the fallback only fires on its
        // failure — no double-report.
        //
        // Position note: the diagnostic uses `Range::default()`
        // (line 0). The AST parameter structs do not carry a source
        // span, so threading a precise location requires a wider
        // change (extend `ArgumentParameter` / `AttributeParameter`
        // / `ParsedExportParam` with a `line: usize` field and update
        // every constructor). The CLI emits the same message without
        // a span — we accept the same trade-off here so the LSP
        // reaches CLI parity in this PR; precise spans can land as a
        // follow-up.
        // Gate on the custom-type load state. The walker itself is unconditional
        // by design (the carina-core helper "always reports unknowns;
        // gating is the caller's choice"); the orphaned-file
        // fallback context turns this flag off so its diagnostics
        // don't over-fire for `arguments {}` blocks in files outside
        // any known project root.
        if self.provider_context.custom_types_loaded() {
            let custom_type_findings = match merged {
                Some(m) => {
                    let mut parsed = m.clone();
                    let mut findings = carina_core::validation::resolve_file_type_exprs(
                        &mut parsed,
                        &self.provider_context,
                    );
                    findings.extend(carina_core::validation::validate_argument_custom_types(
                        &parsed,
                        &self.provider_context,
                    ));
                    findings
                }
                None => current_parsed
                    .map(|p| {
                        let mut parsed = p.clone();
                        let mut findings = carina_core::validation::resolve_file_type_exprs(
                            &mut parsed,
                            &self.provider_context,
                        );
                        findings.extend(carina_core::validation::validate_argument_custom_types(
                            &parsed,
                            &self.provider_context,
                        ));
                        findings
                    })
                    .unwrap_or_default(),
            };
            for msg in custom_type_findings {
                diagnostics.push(carina_diagnostic_range(
                    tower_lsp::lsp_types::Range::default(),
                    tower_lsp::lsp_types::DiagnosticSeverity::ERROR,
                    msg,
                ));
            }
        }

        // Prefer the directory-merged parse so cross-file binding refs
        // resolve; fall back to the buffer-only parse otherwise.
        if let Some(parsed) = merged {
            diagnostics.extend(self.check_depends_on(doc, parsed));
            diagnostics.extend(self.check_wait_bindings(doc, parsed));
            diagnostics.extend(self.check_deferred_populate_refs(doc, parsed));
        } else if let Some(parsed) = current_parsed {
            diagnostics.extend(self.check_depends_on(doc, parsed));
            diagnostics.extend(self.check_wait_bindings(doc, parsed));
            diagnostics.extend(self.check_deferred_populate_refs(doc, parsed));
        }

        // Provider-attribute finalize diagnostic (#2717 / #2753): if a
        // non-literal `default_tags` (or future deferred attribute) does
        // not resolve to the expected shape, surface that error as a
        // diagnostic so editors flag it the same way `carina validate`
        // does.
        if let Some(base) = base_path
            && let Some(err) = self.finalize_provider_diagnostic(doc, current_file_name, base)
        {
            diagnostics.push(parse_error_to_diagnostic(&err));
        }

        // `known_bindings` for the text-scan undefined-reference check.
        // When the merged parse succeeds, `BindingNameSet::from_parsed`
        // yields every binding declared anywhere in the directory. When
        // it fails (`merged = None` because a parse error somewhere in
        // the directory blocked the merge), fall back to a per-file
        // resilient walk: parse each sibling `.crn` independently,
        // swallow per-file errors, run `BindingNameSet::from_parsed` on
        // each successful parse. Without this, sibling-defined
        // bindings (including `arguments`, `import`, `fn`, ...) redline
        // as Unknown the moment any sibling fails to parse (#2445).
        // When no `base_path` is available at all, the buffer-only scan
        // remains the last resort.
        let declared_providers = self.extract_declared_provider_names(&text);
        let known_bindings: HashSet<String> = match (merged, base_path) {
            (Some(merged), _) => carina_core::binding_index::BindingNameSet::from_parsed(merged)
                .iter_names()
                .map(String::from)
                .collect(),
            (None, Some(base)) => {
                self.collect_sibling_binding_names(&text, current_file_name, base)
            }
            (None, None) => current_parsed.map_or_else(
                || self.extract_resource_bindings(crate::completion::DslSource::BufferOnly(&text)),
                |parsed| {
                    carina_core::binding_index::BindingNameSet::from_parsed(parsed)
                        .iter_names()
                        .map(String::from)
                        .collect()
                },
            ),
        };
        diagnostics.extend(self.check_undefined_references(
            &text,
            &known_bindings,
            &declared_providers,
        ));

        // Semantic analysis on parsed file
        if let Some(parsed) = current_parsed {
            // Check provider in module
            diagnostics.extend(self.check_provider_in_module(doc, parsed, merged));

            // Check state blocks in module
            diagnostics.extend(self.check_state_blocks_in_module(doc, parsed, merged));

            // Check backend in module
            diagnostics.extend(self.check_backend_in_module(doc, parsed, merged));

            // Check backend type, attributes, and required settings.
            diagnostics.extend(self.check_backend_config(
                doc,
                merged_result.as_ref(),
                current_file_name,
            ));

            // Check upstream state declarations in module
            diagnostics.extend(self.check_upstream_states_in_module(doc, parsed, merged));

            // Check exports blocks in module
            diagnostics.extend(self.check_exports_in_module(doc, parsed, merged));

            // Check arguments in root. The "is this a root config?"
            // signal — a sibling `backend` block — can live in another
            // .crn file in the same directory, so the check needs the
            // merged directory parse when available (#2198).
            diagnostics.extend(self.check_arguments_in_root(doc, parsed, merged));

            // Check provider region
            diagnostics.extend(self.check_provider_region(
                doc,
                parsed,
                &provider_validation_deduplicator,
            ));

            // Check for unloaded providers
            diagnostics.extend(self.check_unloaded_providers(doc, parsed));

            if let Some(base) = base_path {
                diagnostics.extend(self.check_upstream_state_sources(doc, parsed, base));
            }
            // Build the canonical validation-target index from the merged,
            // module-expanded parse when available. The checks still walk the
            // buffer-only AST for source anchoring, but their binding authority
            // must include sibling resources and expanded compositions. Fall
            // back to the buffer parse only when directory merging failed. This
            // deliberately gives both composition-attribute existence checks
            // and `check_resource_ref_type_mismatch` directory-wide targets.
            let binding_input = merged.unwrap_or(parsed);
            let semantic_view =
                checks::SemanticView::for_analysis(parsed, merged, base_path.is_some());
            let empty_upstream_exports = Default::default();
            let upstream_exports = upstream_resolution
                .as_ref()
                .map(|(exports, _)| exports)
                .unwrap_or(&empty_upstream_exports);
            let module_signatures = base_path
                .map(|base| {
                    carina_core::module_resolver::load_resolved_module_signatures(
                        binding_input,
                        base,
                        &self.provider_context,
                    )
                })
                .unwrap_or_default();
            let binding_index = carina_core::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures(
                binding_input,
                &self.schemas,
                upstream_exports,
                &module_signatures,
            );
            let inference_bindings =
                carina_core::validation::inference::bindings_from_parsed(binding_input);
            let binding_type_context = carina_core::validation::ValidationTypeContext::new(
                binding_input,
                &self.schemas,
                &self.provider_context,
                &module_signatures,
            );
            let binding_use_report = carina_core::validation::check_bare_binding_uses(
                binding_input,
                &binding_type_context,
            );
            // A direct bare binding occupies the complete attribute value, so
            // the shared authority owns that sink even when it succeeds. In
            // particular, an enum-typed argument must not be reinterpreted by
            // the LSP's legacy BindingIndex pass using a differently enriched
            // bootstrap identity. Nested uses keep the surrounding schema
            // walk so unrelated sibling leaves are still validated.
            let binding_error_sinks = parsed
                .bare_binding_uses()
                .uses()
                .filter(|use_site| {
                    use_site.path.segments().len() == 1
                        || binding_use_report.findings().iter().any(|finding| {
                            finding.use_site.source_file == use_site.source_file
                                && finding.use_site.span == use_site.span
                                && finding.use_site.name == use_site.name
                        })
                })
                .filter_map(|use_site| match &use_site.owner {
                    carina_core::parser::BindingUseOwner::Resource(address) => Some((
                        address.resource_id().clone(),
                        use_site.path.root_name().to_string(),
                    )),
                    carina_core::parser::BindingUseOwner::ModuleArgument { .. } => None,
                })
                .collect::<HashSet<_>>();
            let pending_resource_sinks = parsed
                .bare_binding_uses()
                .pending_slots()
                .filter_map(|slot| match &slot.owner {
                    carina_core::parser::BindingUseOwner::Resource(address) => Some((
                        address.resource_id().clone(),
                        slot.path.root_name().to_string(),
                    )),
                    carina_core::parser::BindingUseOwner::ModuleArgument { .. } => None,
                })
                .collect::<HashSet<_>>();
            let diagnostic_file = current_file_path.unwrap_or_else(|| doc.source_file());
            for finding in binding_use_report.findings() {
                if finding.use_site.source_file.path() != diagnostic_file {
                    continue;
                }
                let Some(range) = parser_span_range(doc, finding.use_site.span) else {
                    continue;
                };
                let severity = DiagnosticSeverity::ERROR;
                if let Some(type_error) = finding.error.type_error()
                    && let Some(mut diagnostic) =
                        build_enum_diagnostic_from_error(type_error, Some(range))
                {
                    diagnostic.severity = Some(severity);
                    diagnostics.push(diagnostic);
                } else {
                    diagnostics.push(Diagnostic {
                        range,
                        severity: Some(severity),
                        source: Some("carina".to_string()),
                        message: finding.error.to_string(),
                        ..Default::default()
                    });
                }
            }
            let semantic_file = current_file_path.and_then(|path| {
                merged_result
                    .as_ref()
                    .and_then(|result| result.directory.semantic_file(path))
            });
            let export_value_bindings =
                carina_core::binding_index::ResolvedBindings::for_validation(binding_input);
            diagnostics.extend(self.check_user_function_ref_types(doc, parsed, &binding_index));
            let mut module_call_diagnostics = Vec::new();
            if let Some(base_path) = base_path {
                module_call_diagnostics.extend(self.check_module_calls(
                    parsed,
                    &semantic_view,
                    &module_signatures,
                    &inference_bindings,
                    &binding_use_report,
                ));
                let constraint_diagnostics = current_file_path
                    .zip(merged_result.as_ref())
                    .map(|(current_file_path, result)| {
                        self.module_constraint_diagnostics(
                            current_file_path,
                            &result.module_constraint_reports,
                        )
                    })
                    .unwrap_or_default();
                let has_source_owned_constraint_diagnostic = !constraint_diagnostics.is_empty();
                module_call_diagnostics.extend(constraint_diagnostics);
                if !has_source_owned_constraint_diagnostic
                    && let Some(error) = merged_result
                        .as_ref()
                        .and_then(|result| result.module_error.as_ref())
                {
                    diagnostics.extend(self.module_resolver_error_diagnostics(
                        doc,
                        parsed,
                        error,
                        base_path,
                        current_file_path,
                    ));
                }
            }
            let expansion_succeeded = merged_result
                .as_ref()
                .is_some_and(|result| result.module_error.is_none());
            // Successful expansion supplies inferred types for unannotated
            // forwarded outputs. Declared module outputs were already checked
            // by the mandatory graph pass; both branches use BindingIndex's
            // single TypeInSchema relation.
            if let (Some(_), Some(expanded), true) = (base_path, merged, expansion_succeeded) {
                module_call_diagnostics.extend(self.check_composition_ref_types(
                    parsed,
                    &semantic_view,
                    &expanded.compositions,
                    &binding_index,
                ));
            } else {
                module_call_diagnostics.extend(self.check_module_call_ref_types(
                    parsed,
                    &semantic_view,
                    &module_signatures,
                    &binding_index,
                ));
            }
            diagnostics.extend(deduplicate_identical_diagnostics(module_call_diagnostics));
            // The document AST intentionally preserves function calls for
            // source-oriented checks. Build a parallel, same-order view with
            // locally resolvable calls evaluated so schema diagnostics see
            // the actual value type (not a deferred FunctionCall). The value
            // itself carries any diagnostic-sensitivity provenance.
            let resolved_buffer = {
                let mut resolved = parsed.clone();
                match carina_core::parser::resolve_resource_refs_with_config(
                    &mut resolved,
                    &self.provider_context,
                ) {
                    Ok(()) => Some(resolved),
                    Err(error) => {
                        if matches!(&error, ParseError::SecretDerivedBackendAttribute { .. }) {
                            diagnostics.push(parse_error_to_diagnostic(&error));
                        }
                        None
                    }
                }
            };
            let resolved_buffer_resources = resolved_buffer.as_ref().map(|resolved| {
                resolved
                    .iter_all_resources()
                    .collect::<Vec<carina_core::parser::ResourceRef<'_>>>()
            });
            let mut anonymous_resource_lines = HashMap::<(String, String), VecDeque<u32>>::new();
            if let Ok(spans) = carina_core::parser::top_level_anonymous_resource_spans(&doc.text())
            {
                for authored in spans {
                    anonymous_resource_lines
                        .entry((authored.provider, authored.resource_type))
                        .or_default()
                        .push_back(authored.span.start_line.saturating_sub(1) as u32);
                }
            }
            // Check resource types — include for-body template resources so
            // attribute/type/enum validation fires inside `for` loops too.
            for (resource_index, rref) in parsed.iter_all_resources().enumerate() {
                let ctx = rref.context();
                // carina#3181: schema lookup routes by the typestate arm.
                // The `id` / `attributes` / `quoted_string_attrs` reads
                // below go through the shared `ResourceRef` accessors.
                let resource_id = rref.id();
                let resource_attributes = rref.attributes();
                let resolved_resource_attributes = resolved_buffer_resources
                    .as_ref()
                    .and_then(|resources| resources.get(resource_index))
                    .map(carina_core::parser::ResourceRef::attributes);
                // Directory-aware semantic ASTs can expand a sibling-backed
                // `for` before the buffer-only AST does, so positional
                // resource indexes are not a stable join key. Named bindings
                // are parser-enforced identities. Anonymous resources have no
                // stable cross-AST key and therefore keep the buffer value.
                let semantic_resource_attributes = semantic_file.and_then(|parsed| {
                    rref.binding()
                        .and_then(|binding| {
                            parsed
                                .iter_all_resources()
                                .find(|candidate| candidate.binding() == Some(binding))
                        })
                        .map(|candidate| candidate.attributes())
                });
                let resource_quoted_string_attrs = rref.quoted_string_attrs();
                let resource_schema = || match rref {
                    carina_core::parser::ResourceRef::Resource(m)
                    | carina_core::parser::ResourceRef::Deferred { resource: m, .. } => {
                        self.schemas.get_for(m)
                    }
                    carina_core::parser::ResourceRef::DataSource(d) => {
                        self.schemas.get_for_data_source(d)
                    }
                    carina_core::parser::ResourceRef::LiveComposition(_) => None,
                };
                let provider = &resource_id.provider;
                let full_resource_type = format!("{}.{}", provider, resource_id.resource_type);
                let authored_direct_start = match rref {
                    ResourceRef::Resource(resource) if resource.binding.is_none() => {
                        anonymous_resource_lines
                            .get_mut(&(provider.to_string(), resource_id.resource_type.clone()))
                            .and_then(VecDeque::pop_front)
                    }
                    _ => None,
                };
                // Source-range hint for attribute position lookups. Without
                // this, a `for`-body diagnostic like `mode = "aaa"` would
                // anchor on the first top-level `mode =` in the file.
                let scope = resource_source_range(
                    doc,
                    ctx,
                    provider,
                    &resource_id.resource_type,
                    authored_direct_start,
                );

                if resource_schema().is_none() {
                    let provider_loaded = self.provider_names.contains(&provider.to_string());

                    if let Some(reason) = self.provider_errors.get(provider) {
                        // Provider failed to load
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource_id.resource_type,
                        ) {
                            let end_col = col
                                + resource_id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::INFORMATION,
                                format!("Provider '{}' is not loaded: {}", provider, reason),
                            ));
                        }
                    } else if self.known_provider_names.contains(provider) {
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource_id.resource_type,
                        ) {
                            let end_col = col
                                + resource_id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::ERROR,
                                format!("Unknown resource type: {full_resource_type}"),
                            ));
                        }
                    } else if !provider_loaded {
                        // This directory-scoped provider state has neither a load error
                        // nor a loaded factory for the provider. It does not prove the
                        // provider is absent from the workspace: parse failures, scope
                        // boundaries, and the known discovery gap in carina#3820 can
                        // also leave it out of this file's configuration.
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource_id.resource_type,
                        ) {
                            let end_col = col
                                + resource_id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::ERROR,
                                format!(
                                    "Provider '{}' has no loaded schema for this file: no provider block for it was found in this file's configuration.",
                                    provider
                                ),
                            ));
                        }
                    } else {
                        // Provider loaded but no schema for this resource type
                        if let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource_id.resource_type,
                        ) {
                            let end_col = col
                                + resource_id.resource_type.len() as u32
                                + provider.len() as u32
                                + 1;
                            diagnostics.push(carina_diagnostic(
                                line,
                                col,
                                end_col,
                                DiagnosticSeverity::WARNING,
                                format!(
                                    "No schema for {}.{} — attribute validation skipped",
                                    provider, resource_id.resource_type
                                ),
                            ));
                        }
                    }
                }

                // Semantic validation using schema
                let validated_schema = resource_schema();
                if let Some(validated_schema) = validated_schema {
                    let schema = validated_schema.as_schema();
                    // Check data source without `read` keyword
                    if schema.is_data_source()
                        && !matches!(rref, ResourceRef::DataSource(_))
                        && let Some((line, col)) = self.find_resource_type_position(
                            doc,
                            provider,
                            &resource_id.resource_type,
                        )
                    {
                        let end_col = col
                            + resource_id.resource_type.len() as u32
                            + provider.len() as u32
                            + 1;
                        diagnostics.push(carina_diagnostic(
                            line,
                            col,
                            end_col,
                            DiagnosticSeverity::ERROR,
                            format!(
                                "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                                full_resource_type, full_resource_type
                            ),
                        ));
                    }
                }
                if let Some(validated_schema) = validated_schema {
                    let schema = validated_schema.as_schema();
                    // Build block_name -> canonical_name map for this schema
                    let bn_map = schema.block_name_map();
                    for (attr_name, authored_attr_value) in resource_attributes.iter() {
                        let attr_value = if contains_function_call(authored_attr_value) {
                            resolved_resource_attributes
                                .as_ref()
                                .and_then(|attributes| attributes.get(attr_name))
                                .unwrap_or(authored_attr_value)
                        } else {
                            authored_attr_value
                        };
                        // Coalesce validation must inspect both authored
                        // operands. A directory semantic view may already
                        // have selected one branch, which would erase a
                        // wrong-typed lazy default from diagnostics.
                        let typecheck_value = if matches!(
                            attr_value,
                            Value::Deferred(DeferredValue::Coalesce { .. })
                        ) {
                            attr_value
                        } else {
                            semantic_resource_attributes
                                .as_ref()
                                .and_then(|attributes| attributes.get(attr_name))
                                .unwrap_or(attr_value)
                        };
                        // Resolve block_name to canonical attribute name
                        let canonical_name = bn_map
                            .get(attr_name)
                            .map(|s| s.as_str())
                            .unwrap_or(attr_name);

                        // Check for mixed syntax: both block_name and canonical name present
                        // Skip when block_name == canonical name (singular names like "statement")
                        if let Some(canon) = bn_map.get(attr_name)
                            && canon != attr_name
                            && resource_attributes.contains_key(canon)
                        {
                            if let Some((line, col)) =
                                self.find_attribute_position(doc, attr_name, scope)
                            {
                                diagnostics.push(carina_diagnostic(
                                    line,
                                    col,
                                    col + attr_name.len() as u32,
                                    DiagnosticSeverity::ERROR,
                                    format!(
                                        "Cannot use both '{}' and '{}' (they refer to the same attribute)",
                                        attr_name, canon
                                    ),
                                ));
                            }
                            continue;
                        }

                        // Check for unknown attributes
                        if !schema.attributes().contains_key(canonical_name) {
                            if let Some((line, col)) =
                                self.find_attribute_position(doc, attr_name, scope)
                            {
                                // Check if there's a similar attribute (e.g., vpc -> vpc_id)
                                let suggestion = if schema
                                    .attributes()
                                    .contains_key(&format!("{}_id", attr_name))
                                {
                                    format!(". Did you mean '{}_id'?", attr_name)
                                } else {
                                    String::new()
                                };

                                diagnostics.push(carina_diagnostic(
                                    line,
                                    col,
                                    col + attr_name.len() as u32,
                                    DiagnosticSeverity::WARNING,
                                    format!(
                                        "Unknown attribute '{}' for resource type '{}'{}",
                                        attr_name, resource_id.resource_type, suggestion
                                    ),
                                ));
                            }
                            continue;
                        }

                        // Type validation
                        if let Some(attr_schema) = schema.attributes().get(canonical_name) {
                            let effective_attribute = validated_schema
                                .effective_attribute(canonical_name)
                                .expect("validated schema attributes must have effective views");
                            let effective_type = effective_attribute.value_type();
                            // The shared resource-level validator below owns read-only
                            // assignments. Skip type and authoring guidance here so it
                            // emits the single writability diagnostic, matching core.
                            if effective_attribute.is_read_only() {
                                continue;
                            }

                            if binding_error_sinks
                                .contains(&(resource_id.clone(), attr_name.clone()))
                            {
                                continue;
                            }

                            // Check for block syntax on bare Struct attributes:
                            // Block syntax produces Value::Concrete(ConcreteValue::List), but bare Struct requires
                            // map assignment syntax: attr = { ... }.
                            // Use `Shape` (Ref-peeled) so a Ref-typed
                            // bare-struct attribute is still caught (same
                            // bug class as carina#3349).
                            if matches!(
                                effective_type.shape(),
                                carina_core::schema::Shape::Struct { .. }
                            ) && matches!(attr_value, Value::Concrete(ConcreteValue::List(_)))
                            {
                                let search_name = attr_schema.block_name().unwrap_or(attr_name);
                                let block_positions =
                                    self.find_all_block_positions(doc, search_name);
                                for pos in &block_positions {
                                    diagnostics.push(carina_diagnostic(
                                        pos.0,
                                        pos.1,
                                        pos.1 + search_name.len() as u32,
                                        DiagnosticSeverity::ERROR,
                                        format!(
                                            "'{}' cannot use block syntax; use map assignment: {} = {{ ... }}",
                                            search_name, search_name
                                        ),
                                    ));
                                }
                            }

                            // #2309: direct Enum attributes use the dedicated
                            // structured diagnostic path. Union attributes stay in
                            // the guarded match below so deferred values keep #2847's
                            // behavior and static values are validated only once.
                            if matches!(
                                effective_type.shape(),
                                carina_core::schema::Shape::Enum { .. }
                            ) && let Some(diag) = build_enum_diagnostic(
                                &effective_attribute,
                                attr_value,
                                attr_name,
                                resource_quoted_string_attrs.contains(attr_name),
                                self.find_attribute_value_range(doc, attr_name, scope),
                            ) {
                                diagnostics.push(diag);
                                // Continue past the generic match: this attribute
                                // is already reported with the richer diagnostic.
                                continue;
                            }
                            // Use `Shape` to peel `AttributeType::Ref` against
                            // `schema.defs` at the type-system level. Shape
                            // intentionally has no `Ref` variant — the
                            // wildcard arm cannot silently drop a Ref-typed
                            // attribute (carina#3349 invariant lifted into
                            // the type system).
                            let attr_shape = effective_type.shape();
                            let type_error = match (attr_shape, typecheck_value) {
                                // Bool type should not receive String
                                (
                                    carina_core::schema::Shape::Bool,
                                    Value::Concrete(ConcreteValue::String(_)),
                                ) => Some(format!(
                                    "Type mismatch: expected Bool, got {}. Use true or false.",
                                    carina_core::ValueDescription::of(attr_value)
                                )),
                                // Int type should not receive String
                                (
                                    carina_core::schema::Shape::Int { .. },
                                    Value::Concrete(ConcreteValue::String(_)),
                                ) => Some(format!(
                                    "Type mismatch: expected Int, got {}.",
                                    carina_core::ValueDescription::of(attr_value)
                                )),
                                // Float type should not receive String
                                (
                                    carina_core::schema::Shape::Float { .. },
                                    Value::Concrete(ConcreteValue::String(_)),
                                ) => Some(format!(
                                    "Type mismatch: expected Float, got {}.",
                                    carina_core::ValueDescription::of(attr_value)
                                )),
                                // The shared authority chooses BindingIndex
                                // for canonical reference paths and inference
                                // for computed Access values. Callers do not
                                // classify binding kinds independently.
                                (_, value @ Value::Deferred(DeferredValue::Coalesce { .. })) => {
                                    effective_attribute
                                        .validate(value)
                                        .err()
                                        .map(|error| error.with_attribute(attr_name).to_string())
                                        .or_else(|| {
                                            carina_core::validation::check_deferred_access_type(
                                                value,
                                                schema.type_in_schema(attr_schema.attr_type()),
                                                &inference_bindings,
                                                &self.schemas,
                                                &binding_index,
                                            )
                                            .err()
                                            .map(|error| error.to_string())
                                        })
                                }
                                (
                                    _,
                                    value @ Value::Deferred(
                                        DeferredValue::ResourceRef { .. }
                                        | DeferredValue::Access(_),
                                    ),
                                ) => carina_core::validation::check_deferred_access_type(
                                    value,
                                    schema.type_in_schema(attr_schema.attr_type()),
                                    &inference_bindings,
                                    &self.schemas,
                                    &binding_index,
                                )
                                .err()
                                .map(|error| error.to_string()),
                                (_, Value::Deferred(DeferredValue::BindingRef { binding })) => {
                                    check_resource_ref_type_mismatch(
                                        &binding_index,
                                        schema.type_in_schema(attr_schema.attr_type()),
                                        binding.as_str().into(),
                                    )
                                }
                                (
                                    _,
                                    Value::Deferred(DeferredValue::Unknown(
                                        carina_core::resource::UnknownReason::ModuleArgument {
                                            name,
                                            ..
                                        },
                                    )),
                                ) => check_resource_ref_type_mismatch(
                                    &binding_index,
                                    schema.type_in_schema(attr_schema.attr_type()),
                                    name.as_str().into(),
                                ),
                                // Enum: the structured-payload path
                                // above handles `InvalidEnumVariant` /
                                // `StringLiteralExpectedEnum` and
                                // `continue`s past this match. We only
                                // reach this arm when that path declined
                                // (e.g. `TypeMismatch` from a non-string
                                // value, or `ValidationFailed` from the
                                // namespace-shape fallback) — surface the
                                // plain message so the user still sees a
                                // diagnostic, just without the quick-fix.
                                (carina_core::schema::Shape::Enum { .. }, value) => {
                                    effective_attribute.validate(value).err().map(|e| {
                                        let tagged = e.with_attribute(attr_name);
                                        let reshaped =
                                            if resource_quoted_string_attrs.contains(attr_name) {
                                                tagged.into_string_literal_diagnostic()
                                            } else {
                                                tagged
                                            };
                                        reshaped.to_string()
                                    })
                                }
                                // The shared whole-schema pass below validates identified
                                // scalars with the owner-bound lookup. Doing it here as well
                                // would execute native or bridged callbacks twice per edit.
                                (
                                    carina_core::schema::Shape::String {
                                        identity: Some(_identity),
                                        ..
                                    },
                                    _value,
                                )
                                | (
                                    carina_core::schema::Shape::Int {
                                        identity: Some(_identity),
                                        ..
                                    },
                                    _value,
                                )
                                | (
                                    carina_core::schema::Shape::Float {
                                        identity: Some(_identity),
                                        ..
                                    },
                                    _value,
                                ) => None,
                                // Validate List item types (non-Struct items only;
                                // List<Struct> is handled by validate_struct_value below).
                                // Use `Shape` on the inner so a
                                // `List<Ref<Struct>>` correctly defers to
                                // the struct-validation pass (carina#3349).
                                (
                                    carina_core::schema::Shape::List { .. },
                                    Value::Concrete(ConcreteValue::List(_)),
                                ) if !matches!(
                                    effective_type.list_element().map(|inner| inner.shape()),
                                    Some(carina_core::schema::Shape::Struct { .. })
                                ) =>
                                {
                                    effective_attribute
                                        .validate(attr_value)
                                        .err()
                                        .map(|e| e.with_attribute(attr_name).to_string())
                                }
                                // Validate Map value types
                                (
                                    carina_core::schema::Shape::Map { .. },
                                    Value::Concrete(ConcreteValue::Map(_)),
                                ) => effective_attribute
                                    .validate(attr_value)
                                    .err()
                                    .map(|e| e.with_attribute(attr_name).to_string()),
                                // Validate Union static values (non-ResourceRef, non-BindingRef).
                                // Ref-shaped values are unresolved at this
                                // checkpoint and must not be type-checked
                                // against Union members here — see #2847
                                // for the bare-binding (`BindingRef`) form.
                                (carina_core::schema::Shape::Union, value)
                                    if !matches!(
                                        value,
                                        Value::Deferred(DeferredValue::ResourceRef { .. })
                                            | Value::Deferred(DeferredValue::BindingRef { .. })
                                            | Value::Deferred(DeferredValue::Coalesce { .. })
                                    ) =>
                                {
                                    effective_attribute.validate(value).err().and_then(|error| {
                                        let tagged = error.with_attribute(attr_name);
                                        if let Some(diagnostic) = build_enum_diagnostic_from_error(
                                            &tagged,
                                            self.find_attribute_value_range(doc, attr_name, scope),
                                        ) {
                                            diagnostics.push(diagnostic);
                                            None
                                        } else {
                                            Some(tagged.to_string())
                                        }
                                    })
                                }
                                _ => None,
                            };
                            if let Some(message) = type_error {
                                // Multiple resources of the same type can
                                // share an imprecise parser scope. Prefer it,
                                // but fall back to the unique assignment in
                                // the document instead of dropping a real
                                // schema diagnostic.
                                let position = self
                                    .find_attribute_position(doc, attr_name, scope)
                                    .or_else(|| self.find_attribute_position(doc, attr_name, None));
                                if let Some((line, col)) = position {
                                    diagnostics.push(carina_diagnostic(
                                        line,
                                        col,
                                        col + attr_name.len() as u32,
                                        DiagnosticSeverity::WARNING,
                                        message,
                                    ));
                                }
                            }

                            // Struct field validation. The wrapper borrows
                            // the schema's `AttributeType` directly so we
                            // don't deep-clone the field set on every
                            // keystroke (LSP `analyze` is on
                            // the per-keystroke hot path).
                            //
                            // Use `Shape` (Ref-peeled) so cyclic-CFN
                            // attributes (e.g.
                            // `awscc.s3.Bucket.lifecycle_configuration:
                            // Ref("LifecycleConfiguration")`) still trigger
                            // struct-field validation. Same bug class as
                            // carina#3349 — a raw match arm would silently
                            // drop `Ref` and skip the entire pass.
                            let is_struct_shape = match effective_type.shape() {
                                carina_core::schema::Shape::Struct { .. } => true,
                                carina_core::schema::Shape::List { .. } => {
                                    effective_type.list_element().is_some_and(|inner| {
                                        matches!(
                                            inner.shape(),
                                            carina_core::schema::Shape::Struct { .. }
                                        )
                                    })
                                }
                                _ => false,
                            };
                            if is_struct_shape {
                                diagnostics.extend(self.validate_struct_value(
                                    doc,
                                    attr_name,
                                    attr_value,
                                    &effective_type,
                                ));
                            }
                        }
                    }

                    // Run resource-level validator (e.g., mutually exclusive required fields)
                    let mut resolved_attrs = rref.resolved_attributes();
                    for value in resolved_attrs.values_mut() {
                        if let Some(evaluated) =
                            carina_core::parser::evaluate_static_value_for_validation(
                                value,
                                &self.provider_context,
                            )
                        {
                            *value = evaluated;
                        }
                    }
                    let is_string_literal =
                        |attr: &str| resource_quoted_string_attrs.contains(attr);
                    if let Err(errors) = validated_schema.validate_with_resolver(
                        &resolved_attrs,
                        &is_string_literal,
                        self.provider_context.custom_type_resolver(),
                    ) {
                        for error in errors {
                            if let carina_core::schema::TypeError::MissingRequired { name } = &error
                                && pending_resource_sinks.iter().any(
                                    |(pending_resource, authored_name)| {
                                        pending_resource == resource_id
                                            && bn_map
                                                .get(authored_name)
                                                .map(String::as_str)
                                                .unwrap_or(authored_name)
                                                == name
                                    },
                                )
                            {
                                continue;
                            }
                            if !provider_validation_deduplicator.should_emit_type_error(&error) {
                                continue;
                            }
                            let deferred_attribute = error.attribute_name();
                            if deferred_attribute.is_some_and(|attribute| {
                                matches!(
                                    resource_attributes.get(attribute),
                                    Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
                                        | Some(Value::Deferred(DeferredValue::BindingRef { .. }))
                                )
                            }) {
                                continue;
                            }
                            // Skip errors that are already reported with precise positions
                            // by the attribute-level checks above. Static `secret(...)`
                            // calls are materialized only for this shared schema pass,
                            // so retain their masked nested errors: the authored AST
                            // seen by the attribute pass was still a FunctionCall.
                            let is_masked_secret_error = error.to_string().contains("(secret)");
                            if !is_masked_secret_error
                                && matches!(
                                    &error,
                                    carina_core::schema::TypeError::BlockSyntaxNotAllowed { .. }
                                        | carina_core::schema::TypeError::TypeMismatch { .. }
                                        | carina_core::schema::TypeError::BindingShadowsEnumMember(_)
                                        | carina_core::schema::TypeError::InvalidEnumVariant { .. }
                                        | carina_core::schema::TypeError::UnknownStructField { .. }
                                        | carina_core::schema::TypeError::UnionStructMismatch { .. }
                                        | carina_core::schema::TypeError::UnionAlternativesMismatch { .. }
                                        | carina_core::schema::TypeError::StructFieldError { .. }
                                        | carina_core::schema::TypeError::ListItemError { .. }
                                        | carina_core::schema::TypeError::MapValueError { .. }
                                )
                            {
                                continue;
                            }
                            // Structured type errors carry the authored attribute through
                            // container wrappers; fall back to the resource only when the
                            // error genuinely has no attribute provenance.
                            let position = error
                                .attribute_name()
                                .and_then(|attr| self.find_attribute_position(doc, attr, scope));
                            let position = position.or_else(|| {
                                self.find_resource_type_position(
                                    doc,
                                    provider,
                                    &resource_id.resource_type,
                                )
                            });
                            if let Some((line, col)) = position {
                                diagnostics.push(carina_diagnostic_range(
                                    Range {
                                        start: Position {
                                            line,
                                            character: col,
                                        },
                                        end: Position {
                                            line,
                                            character: col
                                                + doc
                                                    .text()
                                                    .lines()
                                                    .nth(line as usize)
                                                    .map_or(0, |l| l.trim_end().len() as u32),
                                        },
                                    },
                                    DiagnosticSeverity::ERROR,
                                    error.to_string(),
                                ));
                            }
                        }
                    }

                    // Lint: prefer block syntax for List<Struct> attributes
                    diagnostics.extend(self.check_list_struct_syntax(
                        doc,
                        &resolved_attrs,
                        validated_schema.schema(),
                    ));
                }
            }

            // Check the current buffer's attributes against directory-wide
            // binding authority while keeping diagnostics owned by this file.
            diagnostics.extend(self.check_attributes_blocks(
                doc,
                parsed,
                &semantic_view,
                &binding_index,
                &known_bindings,
                &inference_bindings,
            ));

            diagnostics.extend(self.check_exports_blocks(
                doc,
                parsed,
                &semantic_view,
                &export_value_bindings,
            ));

            // Unused `let` detection. When a directory parse is available,
            // run the core check against its authored view (so references
            // from sibling files count as usage) and filter the result to bindings
            // declared in the current file — the LSP warning surface is
            // per-document. With no base path, parse the current buffer through
            // the same authored-view constructor. A failed directory parse must
            // not fall back to that buffer-only view because it cannot see uses
            // from sibling files.
            let unused_binding_names: Vec<String> = match (merged_result.as_ref(), base_path) {
                (Some(result), _) => {
                    let current_file_bindings: HashSet<String> = parsed
                        .iter_all_resources()
                        .filter_map(|rref| rref.binding().map(str::to_string))
                        .collect();
                    carina_core::validation::check_unused_bindings_with_identity_requirements(
                        &result.directory.authored_bindings,
                        &self.schemas,
                    )
                    .into_iter()
                    .filter(|b| current_file_bindings.contains(b))
                    .collect()
                }
                (None, None) => carina_core::config_loader::AuthoredBindingView::parse_file_source(
                    &text,
                    current_file_path.unwrap_or_else(|| doc.source_file()),
                    &self.provider_context,
                )
                .map(|view| {
                    carina_core::validation::check_unused_bindings_with_identity_requirements(
                        &view,
                        &self.schemas,
                    )
                })
                .unwrap_or_default(),
                (None, Some(_)) => Vec::new(),
            };
            diagnostics.extend(self.unused_binding_diagnostics(doc, unused_binding_names));

            // Surface unused-for-binding warnings (recorded in parsed.warnings by
            // the parser) as LSP diagnostics. Other ParseWarnings (e.g. the
            // upstream_state "validate does not inspect" note) are informational
            // context meant for CLI output only and are not elevated here.
            diagnostics.extend(self.check_unused_for_bindings(doc, parsed));
            diagnostics.extend(self.check_single_quoted_interpolation_warnings(parsed));

            // Check for unknown attributes on resource references (typo detection)
            diagnostics.extend(self.check_resource_ref_attributes(doc, parsed, &binding_index));
        }

        // Check for duplicate attribute keys by parsing this document's source.
        // Invalid source yields no duplicate warnings; its parse error is reported above.
        diagnostics.extend(self.check_duplicate_attrs(doc));

        // Check for direct calls to pipe-preferred functions
        diagnostics.extend(self.check_pipe_preferred_direct_calls(doc));

        // Check for non-snake_case binding names
        diagnostics.extend(self.check_non_snake_case_bindings(doc));

        diagnostics
    }

    /// Check for duplicate attribute keys within the same block.
    fn check_duplicate_attrs(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let duplicates = carina_core::lint::find_duplicate_attrs(&text);

        duplicates
            .into_iter()
            .map(|dup| {
                // Convert 1-indexed line to 0-indexed
                let line = (dup.line - 1) as u32;
                // Convert the detector's 1-indexed character column to LSP's
                // 0-indexed character offset.
                let col = dup.column.saturating_sub(1) as u32;

                carina_diagnostic(
                    line,
                    col,
                    dup.end_column.saturating_sub(1) as u32,
                    DiagnosticSeverity::WARNING,
                    format!(
                        "Duplicate attribute '{}' (first defined on line {}). The last value will be used.",
                        dup.name, dup.first_line
                    ),
                )
            })
            .collect()
    }

    /// Check for direct calls to pipe-preferred functions (info-level).
    fn check_pipe_preferred_direct_calls(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_pipe_preferred_direct_calls(&text);

        warnings
            .into_iter()
            .filter_map(|pw| {
                let line = (pw.line - 1) as u32;
                let line_text = text.lines().nth(pw.line - 1)?;
                let pattern = format!("{}(", pw.name);
                let byte_pos = line_text.find(&pattern)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + pw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Consider using pipe form for '{}': data |> {}(...)",
                        pw.name, pw.name
                    ),
                ))
            })
            .collect()
    }

    /// Elevate "for-loop binding '<name>' is unused" parse warnings to LSP
    /// diagnostics. Other ParseWarnings are intentionally not surfaced here:
    /// informational notes (e.g. upstream_state "validate does not inspect")
    /// belong on the CLI, not as editor squiggles.
    fn check_unused_for_bindings(&self, doc: &Document, parsed: &ParsedFile) -> Vec<Diagnostic> {
        let prefix = "for-loop binding '";
        let text = doc.text();
        parsed
            .warnings
            .iter()
            .filter_map(|w| {
                if w.kind != WarningKind::UnusedForBinding {
                    return None;
                }
                let rest = w.message.strip_prefix(prefix)?;
                let name = rest.split('\'').next()?;
                let line_idx = w.line.checked_sub(1)?;
                let line_text = text.lines().nth(line_idx)?;
                let byte_pos = line_text.find(name)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);
                Some(carina_diagnostic(
                    line_idx as u32,
                    col,
                    col + name.chars().count() as u32,
                    DiagnosticSeverity::WARNING,
                    w.message.clone(),
                ))
            })
            .collect()
    }

    fn check_single_quoted_interpolation_warnings(&self, parsed: &ParsedFile) -> Vec<Diagnostic> {
        parsed
            .warnings
            .iter()
            .filter(|w| w.kind == WarningKind::SingleQuotedInterpolation)
            .filter_map(|w| {
                let span = w.span.as_ref()?;
                let line = span.start_line.checked_sub(1)? as u32;
                let start_col = span.start_column.checked_sub(1)? as u32;
                let end_col = span.end_column.checked_sub(1)? as u32;
                Some(carina_diagnostic(
                    line,
                    start_col,
                    end_col,
                    DiagnosticSeverity::WARNING,
                    w.message.clone(),
                ))
            })
            .collect()
    }

    /// Check for non-snake_case binding names (info-level).
    fn check_non_snake_case_bindings(&self, doc: &Document) -> Vec<Diagnostic> {
        let text = doc.text();
        let warnings = carina_core::lint::find_non_snake_case_bindings(&text);

        warnings
            .into_iter()
            .filter_map(|nw| {
                let line = (nw.line - 1) as u32;
                let line_text = text.lines().nth(nw.line - 1)?;
                // Find the binding name position in the line
                let byte_pos = line_text.find(&nw.name)?;
                let col = position::byte_offset_to_char_offset(line_text, byte_pos);

                Some(carina_diagnostic(
                    line,
                    col,
                    col + nw.name.len() as u32,
                    DiagnosticSeverity::INFORMATION,
                    format!(
                        "Binding '{}' is not snake_case. Use snake_case for binding names (e.g., 'my_resource').",
                        nw.name
                    ),
                ))
            })
            .collect()
    }

    fn find_resource_type_position(
        &self,
        doc: &Document,
        provider: &str,
        resource_type: &str,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let pattern = format!("{}.{}", provider, resource_type);

        for (line_idx, line) in text.lines().enumerate() {
            if let Some(byte_pos) = line.find(pattern.as_str()) {
                return Some((
                    line_idx as u32,
                    position::byte_offset_to_char_offset(line, byte_pos),
                ));
            }
        }
        None
    }

    /// Find the source position of an attribute assignment (`attr_name = ...`).
    ///
    /// `scope` optionally restricts the search to a half-open line range
    /// `[start, end)` (both 0-indexed). Without a scope, the first matching
    /// line anywhere in the document wins — which produces wrong anchors
    /// when the same attribute name appears in multiple resource blocks.
    /// Callers walking resources should pass the resource's source range
    /// (see `resource_source_range`).
    fn find_attribute_position(
        &self,
        doc: &Document,
        attr_name: &str,
        scope: Option<(u32, u32)>,
    ) -> Option<(u32, u32)> {
        let text = doc.text();
        let (start, end) = scope.unwrap_or((0, u32::MAX));

        for (line_idx, line) in text.lines().enumerate() {
            let line_idx = line_idx as u32;
            if line_idx < start {
                continue;
            }
            if line_idx >= end {
                break;
            }
            for (byte_column, _) in line.match_indices(attr_name) {
                let before = line[..byte_column].chars().next_back();
                if before.is_some_and(|ch| !ch.is_whitespace() && ch != '{' && ch != ',') {
                    continue;
                }
                let after = &line[byte_column + attr_name.len()..];
                if !after.trim_start().starts_with('=') {
                    continue;
                }
                return Some((
                    line_idx,
                    position::byte_offset_to_char_offset(line, byte_column),
                ));
            }
        }
        None
    }

    /// Locate the value side of `attr_name = <value>` and return the
    /// `Range` covering the value token. For a quoted string literal
    /// the range covers both surrounding quote characters; for a
    /// dotted identifier the range covers the whole identifier
    /// (including any `.` separators) up to the first whitespace or
    /// line-terminator.
    ///
    /// Used by code-action diagnostics so the editor's quick-fix
    /// `WorkspaceEdit` overwrites just the offending value, not the
    /// whole line.
    ///
    /// Returns `None` for value shapes the LSP code action does not
    /// support: block-syntax `attr = { ... }`, multi-line lists,
    /// heredocs, interpolated strings, compound quoted expressions, or
    /// missing-value lines.
    fn find_attribute_value_range(
        &self,
        doc: &Document,
        attr_name: &str,
        scope: Option<(u32, u32)>,
    ) -> Option<tower_lsp::lsp_types::Range> {
        use tower_lsp::lsp_types::{Position, Range};
        let text = doc.text();
        let (start, end) = scope.unwrap_or((0, u32::MAX));

        for (line_idx, line) in text.lines().enumerate() {
            let line_idx_u32 = line_idx as u32;
            if line_idx_u32 < start {
                continue;
            }
            if line_idx_u32 >= end {
                break;
            }
            let leading = position::leading_whitespace_chars(line) as usize;
            let trimmed = &line[leading..];
            if !trimmed.starts_with(attr_name) {
                continue;
            }
            let after_attr = &trimmed[attr_name.len()..];
            if !after_attr.starts_with(' ')
                && !after_attr.starts_with('\t')
                && !after_attr.starts_with('=')
            {
                continue;
            }
            // Skip past `name`, optional whitespace, `=`, optional whitespace.
            // Carina's formatter standardizes spaces, but accept tabs too so
            // a hand-formatted buffer (e.g. mid-edit) still surfaces the
            // quick-fix instead of silently dropping it.
            let mut col = leading + attr_name.len();
            let bytes = line.as_bytes();
            while col < bytes.len() && (bytes[col] == b' ' || bytes[col] == b'\t') {
                col += 1;
            }
            if col >= bytes.len() || bytes[col] != b'=' {
                return None;
            }
            col += 1; // past `=`
            while col < bytes.len() && (bytes[col] == b' ' || bytes[col] == b'\t') {
                col += 1;
            }
            if col >= bytes.len() {
                return None;
            }
            let value_start = col;
            let value_end = if bytes[col..].starts_with(b"<<") {
                // A heredoc spans multiple lines. Replacing only its `<<MARKER`
                // opener would leave the body and terminator behind.
                return None;
            } else if bytes[col] == b'"' || bytes[col] == b'\'' {
                // Quoted string literal: include both quotes. Bail
                // (return None) if the closing quote is missing rather
                // than guess at a range. Interpolated strings are also
                // excluded because nested expression quotes make this
                // line-oriented scan unable to prove the full token span.
                // Single-quoted strings are literal-only in carina.pest.
                let quote = bytes[col];
                let mut i = col + 1;
                while i < bytes.len() && bytes[i] != quote {
                    // Skip a backslash-escaped quote or backslash so it
                    // doesn't terminate the literal early.
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if quote == b'"' && bytes[i..].starts_with(b"${") {
                        return None;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                if !strip_line_comment(&line[i + 1..]).trim().is_empty() {
                    return None;
                }
                i + 1
            } else if bytes[col] == b'{' || bytes[col] == b'[' {
                // Block-syntax / list-syntax values are out of scope
                // for this code action — there's no single offending
                // identifier to replace.
                return None;
            } else {
                // Bare identifier (possibly dotted, e.g.
                // `aws.s3.Bucket.VersioningStatus.Enabled`). Read up
                // to whitespace or a comment.
                let mut i = col;
                while i < bytes.len()
                    && bytes[i] != b' '
                    && bytes[i] != b'\t'
                    && bytes[i] != b'#'
                    && bytes[i] != b','
                {
                    i += 1;
                }
                i
            };
            // LSP `Position.character` is a UTF-16/codepoint offset, not
            // a byte offset. The byte-walk above is safe up to
            // `value_start` (only ASCII attr-name / whitespace / `=`
            // precede it on the line) but the value itself can contain
            // multi-byte chars (e.g. `attr = "あ"`), so convert both
            // ends through `byte_offset_to_char_offset` before handing
            // them to the LSP client.
            return Some(Range {
                start: Position {
                    line: line_idx_u32,
                    character: position::byte_offset_to_char_offset(line, value_start),
                },
                end: Position {
                    line: line_idx_u32,
                    character: position::byte_offset_to_char_offset(line, value_end),
                },
            });
        }
        None
    }
}

/// Derive the source line range (0-indexed, half-open) of a resource block.
///
/// - For `Deferred(for_expr)` the scope starts at the `for` line and ends
///   at the closing brace of the for expression.
/// - For an authored anonymous `Direct` resource, the parser-derived start
///   line selects that exact declaration. Other direct resources fall back to
///   locating the `provider.resource_type` token.
///
/// Returns `None` when the block can't be located (e.g., during partial
/// parses). Callers that fall back to `None` get document-wide search —
/// the historic behavior — which is still correct when there's no
/// ambiguity.
fn resource_source_range(
    doc: &Document,
    ctx: carina_core::parser::ResourceContext<'_>,
    provider: &str,
    resource_type: &str,
    authored_direct_start: Option<u32>,
) -> Option<(u32, u32)> {
    use carina_core::parser::ResourceContext;
    let text = doc.text();
    let lines: Vec<&str> = text.lines().collect();

    let start = match ctx {
        // `DeferredForExpression.line` is 1-indexed (pest line_col). Convert to 0-indexed.
        ResourceContext::Deferred(d) => d.line.saturating_sub(1) as u32,
        ResourceContext::Direct => {
            if let Some(start) = authored_direct_start {
                start
            } else {
                let pattern = format!("{}.{}", provider, resource_type);
                let (idx, _) = lines
                    .iter()
                    .enumerate()
                    .find(|(_, l)| l.contains(pattern.as_str()))?;
                idx as u32
            }
        }
    };

    // Scan forward from `start`, tracking brace balance. The block ends when
    // the balance returns to zero after we've seen at least one `{`.
    let mut balance: i32 = 0;
    let mut seen_open = false;
    for (idx, line) in lines.iter().enumerate().skip(start as usize) {
        // Skip comments (`#` or `//`) by trimming from the first marker.
        let stripped = strip_line_comment(line);
        for ch in stripped.chars() {
            match ch {
                '{' => {
                    balance += 1;
                    seen_open = true;
                }
                '}' => {
                    balance -= 1;
                    if seen_open && balance == 0 {
                        return Some((start, idx as u32 + 1));
                    }
                }
                _ => {}
            }
        }
    }
    // Unterminated: treat as extending to end of file.
    Some((start, lines.len() as u32))
}

/// Strip a `#` or `//` line comment outside quoted text.
///
/// This supports both resource-block brace balancing and checks that only
/// whitespace or a comment follows a quoted attribute value.
fn strip_line_comment(line: &str) -> &str {
    let mut in_string = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\'' {
            in_string = !in_string;
        } else if !in_string {
            if b == b'#' {
                return &line[..i];
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                return &line[..i];
            }
        }
        i += 1;
    }
    line
}

/// Build a `Diagnostic` for a failed direct `Enum` attribute validation,
/// carrying the structured candidate list on `Diagnostic.data` and a
/// range over the offending value text. Returns `None` when validation
/// passed, when the value range can't be located (block-syntax,
/// missing value), or when the failing error isn't an enum-variant
/// shape (e.g. a `TypeMismatch` from a non-string `Value`).
///
/// See #2309. The structured payload is consumed by
/// [`crate::code_action::code_actions_for_diagnostic`].
fn build_enum_diagnostic(
    effective_attribute: &carina_core::schema::EffectiveAttribute<'_>,
    attr_value: &Value,
    attr_name: &str,
    is_quoted_literal: bool,
    value_range: Option<tower_lsp::lsp_types::Range>,
) -> Option<Diagnostic> {
    let err = effective_attribute.validate(attr_value).err()?;
    let tagged = err.with_attribute(attr_name);
    // Reshape into the shape-mismatch diagnostic when the value came
    // from a quoted string literal — mirrors CLI behavior (#2094).
    let reshaped = if is_quoted_literal {
        tagged.into_string_literal_diagnostic()
    } else {
        tagged
    };
    build_enum_diagnostic_from_error(&reshaped, value_range)
}

pub(super) fn build_enum_diagnostic_from_error(
    error: &carina_core::schema::TypeError,
    value_range: Option<tower_lsp::lsp_types::Range>,
) -> Option<Diagnostic> {
    let data = crate::code_action::enum_diagnostic_data_for_error(error)?;
    let range = value_range?;
    Some(Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("carina".to_string()),
        message: error.to_string(),
        data: Some(serde_json::to_value(data).expect("EnumDiagnosticData serialize")),
        ..Default::default()
    })
}

/// Check whether a ResourceRef value is type-compatible with the expected attribute type.
/// Returns `Some(message)` on mismatch, `None` when compatible or when the binding/attribute
/// cannot be resolved (unknown bindings are not flagged here).
fn check_resource_ref_type_mismatch(
    binding_index: &carina_core::binding_index::BindingIndex<'_>,
    expected: TypeInSchema<'_>,
    path: carina_core::resource::ReferencePathRef<'_>,
) -> Option<String> {
    let carina_core::binding_index::RefType::Typed(source) = binding_index.ref_type(path) else {
        return None;
    };

    // Directional: the ref (source) must be assignable to the expected (sink).
    let mismatch = source.is_assignable_to_sink(expected).err()?;
    Some(format!(
        "Type mismatch: expected {}, got {} ({})",
        expected.resolved_type_name(),
        mismatch.actual_type_name(),
        mismatch.origin_description(path),
    ))
}

fn parse_error_to_diagnostic(error: &ParseError) -> Diagnostic {
    match error {
        ParseError::Located { error, .. } => parse_error_to_diagnostic(error),
        ParseError::Syntax(pest_error) => {
            let (line, col) = match pest_error.line_col {
                pest::error::LineColLocation::Pos((line, col)) => (line, col),
                pest::error::LineColLocation::Span((line, col), _) => (line, col),
            };

            carina_diagnostic(
                (line.saturating_sub(1)) as u32,
                (col.saturating_sub(1)) as u32,
                col as u32,
                DiagnosticSeverity::ERROR,
                format!("{}", pest_error),
            )
        }
        ParseError::InvalidExpression { line, message } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            message.clone(),
        ),
        ParseError::UnknownFunction { name, line, column } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            (*column as u32).saturating_sub(1),
            column.saturating_sub(1) as u32 + name.len() as u32,
            DiagnosticSeverity::ERROR,
            format!("Unknown function '{name}'"),
        ),
        ParseError::ReservedResourceAttribute {
            attribute,
            line,
            column,
            end_line,
            end_column,
        } => carina_diagnostic_range(
            Range {
                start: Position {
                    line: line.saturating_sub(1) as u32,
                    character: column.saturating_sub(1) as u32,
                },
                end: Position {
                    line: end_line.saturating_sub(1) as u32,
                    character: end_column.saturating_sub(1) as u32,
                },
            },
            DiagnosticSeverity::ERROR,
            format!(
                "Reserved attribute name '{attribute}': names starting with '_' are reserved for Carina"
            ),
        ),
        ParseError::SecretDerivedBackendAttribute {
            line,
            start_column,
            end_column,
            ..
        } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            start_column.saturating_sub(1) as u32,
            end_column.saturating_sub(1) as u32,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::InterpolationNotAllowedInLiteralString { line } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            "interpolation is not allowed in this string literal".to_string(),
        ),
        ParseError::InvalidResourceIdentity { line, error } => carina_diagnostic(
            (*line as u32).saturating_sub(1),
            0,
            100,
            DiagnosticSeverity::ERROR,
            error.to_string(),
        ),
        ParseError::UndefinedVariable(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Undefined variable: {}", name),
        ),
        ParseError::InvalidResourceType(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Invalid resource type: {}", name),
        ),
        ParseError::DuplicateModule(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Duplicate module definition: {}", name),
        ),
        ParseError::DuplicateBinding { name, line } => carina_diagnostic(
            (line - 1) as u32,
            0,
            0,
            DiagnosticSeverity::ERROR,
            format!("Duplicate binding: {}", name),
        ),
        err @ ParseError::UndefinedIdentifier { line, .. } => carina_diagnostic(
            line.saturating_sub(1) as u32,
            0,
            0,
            DiagnosticSeverity::ERROR,
            err.to_string(),
        ),
        ParseError::ModuleNotFound(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Module not found: {}", name),
        ),
        ParseError::InternalError { expected, context } => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!(
                "Internal parser error: expected {} in {}",
                expected, context
            ),
        ),
        ParseError::RecursiveFunction(name) => carina_diagnostic_range(
            Range::default(),
            DiagnosticSeverity::ERROR,
            format!("Recursive function call detected: {}", name),
        ),
        err @ ParseError::CyclicLetBinding { .. } => {
            carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, err.to_string())
        }
        ParseError::UserFunctionError(msg) => {
            carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, msg.to_string())
        }
    }
}
