//! Module-call expansion, argument substitution, and intra-module
//! reference rewriting.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use crate::binding_index::{BindingIndex, RefType, RefTypeError};
use crate::eval_value::{BareStructuralHandle, StructuralHandleKind};
use crate::identifier::SimHash;
use crate::parser::{
    ArgumentOrigin, BindingName, DeferredForExpression, ModuleArgument, ModuleCall, ParseError,
    ParsedFile, WaitBinding,
};
use crate::resource::{
    AccessMissPolicy, CompositionCall, CompositionCallPath, ConcreteValue, DataSource,
    DeferredValue, LegacyLoopAddress, LiveComposition, ModuleInstanceScope, ModuleSource,
    PathSegment, Resource, ResourceId, ResourceIdentity, ResourceIdentityState,
    StructuralOperandRef, Value, ValuePathSegmentResult, apply_value_path_segment,
};

use super::error::ModuleError;
use super::resolver::{
    ModuleCallConstraintReport, ModuleResolver, ResolvedModuleConstraintOrigin,
    ResolvedModuleConstraintOutcome, ResolvedModuleConstraintStatus, TypeResolutionPolicy,
};
use super::validation::{
    ConstraintEvaluation, ModuleConstraintKind, ModuleConstraints, evaluate_module_constraints,
};

impl<P: TypeResolutionPolicy> ModuleResolver<'_, P> {
    /// Expand a module call into resources.
    ///
    /// Every module call creates a [`LiveComposition`]. Bound calls expose module
    /// outputs through it; all calls retain their typed argument boundary for
    /// one post-expansion validation pass. Virtual resources are skipped by
    /// the differ.
    ///
    /// Returns the contribution as a concrete `ParsedFile` — module
    /// expansion is a parser-phase operation (it reads parser-phase
    /// module data from `imported_modules`), so the honest return type
    /// is `ParsedFile`, not a phantom `File<E>`.
    ///
    /// carina#3126 root fix: the caller folds this in via the *one*
    /// shared [`merge_parsed_file`](crate::config_loader) — there is no
    /// second hand-maintained field list that can silently diverge
    /// from `File<E>` (the carina#3061 / carina#3126 bug class: a
    /// module-internal `wait` / `for` dropped because the expansion
    /// path forgot a field). The contribution is built below with an
    /// **exhaustive struct literal**, so a new `File<E>` field cannot
    /// compile until it is classified as "populated from the module
    /// (instance-prefixed)" or "consumed during expansion, not
    /// propagated". The generic-`File<E>` caller
    /// (`resolve_modules_with_config<E>`) routes the contribution
    /// through [`relabel_export_phase`](crate::config_loader) before
    /// the merge so it stays phase-agnostic (today every caller is
    /// `E = ParsedExportParam`, making that a same-phase no-op;
    /// module loading rejects `export_params` before expansion, so the
    /// empty contribution and its relabel are enforced by the module
    /// boundary). The recursive parser-phase caller
    /// (`resolve_nested_modules`) merges directly.
    pub(super) fn expand_module_call_checked(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
        ref_check: &super::resolver::RefCheckWitness,
        binding_index: Option<&BindingIndex<'_>>,
        resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    ) -> Result<ParsedFile, ModuleError> {
        self.expand_module_call_inner(
            call,
            instance_prefix,
            ref_check,
            binding_index,
            resolved_bindings,
        )
        .map_err(|error| error.with_failing_call(call.id().clone()))
    }

    fn expand_module_call_inner(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
        _ref_check: &super::resolver::RefCheckWitness,
        binding_index: Option<&BindingIndex<'_>>,
        resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    ) -> Result<ParsedFile, ModuleError> {
        let module = self
            .imported_modules
            .get(&call.module_name)
            .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?;
        let call_source_file = call.source_file();
        let call_source_path = crate::utils::normalize_source_path(call_source_file)
            .display()
            .to_string();
        let contextualize_access_error = |error: ArgumentSubstitutionError| {
            let ArgumentSubstitutionError {
                source_file,
                line,
                message,
                parse_error,
            } = error;
            if let Some(error) = parse_error {
                return ModuleError::Parse(*error);
            }
            ModuleError::ArgumentValueAccess {
                module: call.module_name.clone(),
                source_path: source_file
                    .map(|path| {
                        crate::utils::normalize_source_path(&path)
                            .display()
                            .to_string()
                    })
                    .or_else(|| {
                        self.module_paths.get(&call.module_name).map(|path| {
                            crate::utils::normalize_source_path(std::path::Path::new(path))
                                .display()
                                .to_string()
                        })
                    })
                    .unwrap_or_else(|| call_source_path.clone()),
                line,
                message,
            }
        };

        // Validate required arguments
        for arg in &module.arguments {
            if arg.default.is_none() && !call.contains_argument(&arg.name) {
                return Err(ModuleError::MissingArgument {
                    module: call.module_name.clone(),
                    argument: arg.name.clone(),
                });
            }
        }

        // Validate no unknown arguments
        let declared_arg_names: HashSet<&str> =
            module.arguments.iter().map(|a| a.name.as_str()).collect();
        for (arg_name, _) in call.arguments() {
            if !declared_arg_names.contains(arg_name) {
                return Err(ModuleError::UnknownArgument {
                    module: call.module_name.clone(),
                    argument: arg_name.to_string(),
                });
            }
        }

        // Build argument value map.
        //
        // Defaults may interpolate other arguments (#2393), e.g.
        // `subject_patterns: list(String) = ["repo:${github_repo}:*"]`.
        // The parser registers each argument as a typed module-argument
        // placeholder, so defaults retain lexical references without
        // confusing value parameters with structural resource addresses.
        //
        // **Initial pass** seeds each entry with either the caller-
        // supplied value or the raw default. **Fix-point loop** then
        // re-substitutes every entry against the current map until an
        // iteration produces no change — this lets a forward-ref
        // default like `prefix: String = later` (where `later` is
        // declared after `prefix`) resolve to `later`'s value once
        // `later` itself is resolved. A hard iteration cap (one round
        // per argument plus one) bounds genuinely cyclic shapes
        // (`a = b`, `b = a`); leftover unresolved refs surface to the
        // post-merge scope check as undefined identifiers. #2817.
        let mut resolved_arguments: HashMap<String, ResolvedArgument> = HashMap::new();
        let call_line = call.source_span().start_line();
        for arg in &module.arguments {
            let argument = match call.authored_argument(&arg.name) {
                Some(authored) => ResolvedArgument {
                    argument: authored.argument().clone(),
                    origin: ArgumentOrigin::Authored {
                        key_span: authored.key_span(),
                        value_span: authored.value_span(),
                    },
                },
                None => ResolvedArgument {
                    argument: ModuleArgument::new(
                        arg.default
                            .clone()
                            .expect("required arguments were checked above"),
                    ),
                    origin: ArgumentOrigin::Defaulted,
                },
            };
            resolved_arguments.insert(arg.name.clone(), argument);
        }
        let caller_supplied_arguments: HashSet<&str> =
            call.arguments().map(|(name, _)| name).collect();
        let max_iterations = module.arguments.len() + 1;
        for _ in 0..max_iterations {
            let mut changed = false;
            // Snapshot the current map so each entry resolves against
            // the same generation — without this, the order in which
            // we iterate `module.arguments` would silently re-introduce
            // declaration-order coupling.
            let snapshot = resolved_arguments.clone();
            let substitutions = ArgumentSubstitutions {
                arguments: &snapshot,
                default_line: call_line,
                caller_source_file: call_source_file,
                binding_index,
                config: self.config,
            };
            for arg in &module.arguments {
                // Call-site expressions are evaluated in the caller's scope,
                // not against the callee's parameter namespace. In particular,
                // `peer = peer.identifier` must never substitute the argument
                // into itself on each fixed-point round.
                if caller_supplied_arguments.contains(arg.name.as_str()) {
                    continue;
                }
                let current = resolved_arguments.get(&arg.name).expect("seeded above");
                if current.handle().is_some() {
                    continue;
                }
                let mut next = substitute_arguments_with_position(
                    current.walker_value(),
                    substitutions,
                    ArgumentSubstitutionPosition::Value,
                )
                .map_err(contextualize_access_error)?;
                next.canonicalize_in_place();
                // A pending parameter's traversal value is its placeholder,
                // not an independently authored access projection. Keeping it
                // as `access_base = Some(..)` would prevent `from_parts` from
                // reclassifying an unresolved default as pending for the next
                // fixed-point round.
                let next_access = if current.pending_parameter().is_some() {
                    None
                } else {
                    current
                        .access_base()
                        .map(|access_base| {
                            let mut access_base = substitute_arguments_with_position(
                                access_base,
                                substitutions,
                                ArgumentSubstitutionPosition::AccessBase,
                            )?;
                            access_base.canonicalize_in_place();
                            Ok::<_, ArgumentSubstitutionError>(access_base)
                        })
                        .transpose()
                        .map_err(contextualize_access_error)?
                };
                let next = ModuleArgument::from_parts(next, next_access);
                if next != current.argument {
                    resolved_arguments.insert(
                        arg.name.clone(),
                        ResolvedArgument {
                            argument: next,
                            origin: current.origin,
                        },
                    );
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let argument_substitutions = ArgumentSubstitutions {
            arguments: &resolved_arguments,
            default_line: call_line,
            caller_source_file: call_source_file,
            binding_index,
            config: self.config,
        };
        let argument_values: HashMap<String, Value> = resolved_arguments
            .iter()
            .map(|(name, argument)| {
                let value = argument.walker_value();
                (name.clone(), value.clone())
            })
            .collect();

        // Type-check argument values against declared types
        for arg in &module.arguments {
            let argument = resolved_arguments.get(&arg.name).unwrap();
            let line = match argument.origin {
                ArgumentOrigin::Authored { value_span, .. } => value_span.start_line(),
                ArgumentOrigin::Defaulted => call_line,
            };
            let Some(resolved_parameter) =
                crate::validation::resolve_parameter_type_for_check(&arg.type_expr, self.config)
                    .map_err(|message| {
                        ModuleError::Parse(ParseError::InvalidExpression { line, message })
                    })?
            else {
                continue;
            };
            match argument.value_ref() {
                StructuralOperandRef::Handle { handle, .. } => {
                    if let Err(error) =
                        crate::validation::check_handle_sink(handle, &resolved_parameter)
                    {
                        return Err(ModuleError::BareStructuralHandleArgument {
                            module: call.module_name.clone(),
                            argument: arg.name.clone(),
                            source_path: call_source_path.clone(),
                            line,
                            message: error.to_string(),
                        });
                    }
                    continue;
                }
                StructuralOperandRef::Parameter { .. } => continue,
                StructuralOperandRef::Value(_) => {}
            }
            let value = argument
                .value()
                .expect("value-shaped module argument was matched above");
            let resolved_type = resolved_parameter.as_type_expr();
            crate::validation::typecheck::check_type_expr_value(
                &resolved_type,
                value,
                self.config,
                crate::validation::typecheck::TypecheckInference::Unavailable,
            )
            .map_err(|mismatch| ModuleError::InvalidArgumentType {
                module: call.module_name.clone(),
                argument: arg.name.clone(),
                mismatch,
            })?;
        }

        // Collect and evaluate argument-local validations and module-level
        // requirements through the single constraint seam shared by the LSP,
        // planning, and apply.
        let bound_constraints =
            bind_module_constraints(module, argument_substitutions, resolved_bindings)
                .map_err(contextualize_access_error)?;
        let evaluated_constraints = evaluate_module_constraints(
            ModuleConstraints::pending(&bound_constraints),
            &argument_values,
        );
        if self.records_constraint_reports() {
            let source = call.diagnostic_source();
            let outcomes = evaluated_constraints
                .iter()
                .map(|evaluated| {
                    let status = match evaluated.evaluation() {
                        ConstraintEvaluation::Satisfied => {
                            ResolvedModuleConstraintStatus::Satisfied
                        }
                        ConstraintEvaluation::Pending => ResolvedModuleConstraintStatus::Pending,
                        ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_) => {
                            let origin = match evaluated.kind() {
                                ModuleConstraintKind::ArgumentValidation { argument } => {
                                    ResolvedModuleConstraintOrigin::Argument(
                                        resolved_arguments
                                            .get(argument)
                                            .expect("validated argument was resolved")
                                            .origin,
                                    )
                                }
                                ModuleConstraintKind::Require => {
                                    ResolvedModuleConstraintOrigin::Call
                                }
                            };
                            ResolvedModuleConstraintStatus::Failed {
                                diagnostic: Box::new(
                                    evaluated
                                        .module_diagnostic(
                                            &call.module_name,
                                            instance_prefix,
                                            call.binding_name.as_deref(),
                                            Some(call.id()),
                                        )
                                        .expect("concrete outcome has a diagnostic"),
                                ),
                                origin,
                            }
                        }
                    };
                    ResolvedModuleConstraintOutcome {
                        id: evaluated.constraint().id().clone(),
                        status,
                    }
                })
                .collect();
            let (identity, report) = ModuleCallConstraintReport::new(source, outcomes);
            self.record_constraint_report(identity, report);
        }

        let mut pending_constraints = Vec::new();
        for evaluated in evaluated_constraints {
            match evaluated.evaluation() {
                ConstraintEvaluation::Satisfied => {}
                ConstraintEvaluation::Pending => {
                    pending_constraints.push(evaluated.into_constraint());
                }
                ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_) => {
                    return Err(evaluated
                        .module_error(
                            &call.module_name,
                            instance_prefix,
                            call.binding_name.as_deref(),
                            Some(call.id()),
                        )
                        .expect("concrete constraint failure has a resolver error"));
                }
            }
        }

        // Collect intra-module binding names so we can rewrite
        // ResourceRefs. Wait-binding names are included alongside
        // resource bindings so a downstream resource referencing
        // `<wait_binding>.<attr>` is instance-prefixed the same way —
        // otherwise its dependency edge to the wait is lost
        // (carina#3061; see `prefix_wait_binding`). Module-call bindings
        // (`let x = inner { ... }`) need the same treatment so a sibling
        // resource referencing `x.<attr>` lines up with the
        // `_virtual.<prefix>.x` row produced by the inner expansion
        // (carina#3243).
        let intra_module_bindings: HashSet<String> = module
            .resources
            .iter()
            .filter_map(|r| r.binding.clone())
            .chain(module.data_sources.iter().filter_map(|d| d.binding.clone()))
            .chain(
                module
                    .wait_bindings
                    .iter()
                    .map(|w| w.binding.as_str().to_string()),
            )
            .chain(
                module
                    .module_calls
                    .iter()
                    .filter_map(|c| c.binding_name.clone()),
            )
            // Nested compositions may already carry a multi-segment binding
            // (`middle.inner`). When this module is itself expanded, refs to
            // that exact binding must gain the new outer prefix as well.
            .chain(module.compositions.iter().filter_map(|c| c.binding.clone()))
            .collect();
        let module_source = self.module_paths.get(&call.module_name).cloned();
        let module_directory = module_source.as_deref().map(|source| {
            let source = std::path::Path::new(source);
            let path = if source.is_absolute() {
                source.to_path_buf()
            } else {
                self.base_dir.join(source)
            };
            path.canonicalize().unwrap_or(path)
        });
        let expanded_call = CompositionCall {
            id: call.id().clone(),
            module_name: call.module_name.clone(),
            binding: call.binding_name.clone(),
            instance: instance_prefix.to_string(),
            module_source: module_source.clone(),
            module_directory,
            loop_origin: call.loop_origin().cloned().map(Box::new),
        };
        let module_instance_scope = ModuleInstanceScope::for_call(
            &call.module_name,
            call.binding_name.as_deref(),
            instance_prefix,
        );

        // Expand managed resources with substituted values.
        let mut resources: Vec<Resource> = Vec::new();
        let mut expanded_parser_metadata = crate::parser::ParserBindingMetadata::default();
        let mut expanded_binding_uses = module.bare_binding_uses().clone();
        for resource in &module.resources {
            let source_file = module
                .parser_binding_metadata
                .resource_source_file(&resource.id);
            let expanded = prefix_module_resource(
                resource,
                instance_prefix,
                &call.module_name,
                &module_instance_scope,
                &expanded_call,
                &intra_module_bindings,
                argument_substitutions,
            )
            .map_err(|error| {
                contextualize_access_error(error.with_optional_source_file(source_file))
            })?;
            if let Some(source_file) = source_file {
                expanded_parser_metadata
                    .insert_resource_source_file(expanded.id.clone(), source_file.to_path_buf());
            }
            expanded_binding_uses.remap_resource_owner(&resource.id, &expanded.id);
            resources.push(expanded);
        }

        // Expand data sources with substituted values.
        let mut data_sources: Vec<DataSource> = Vec::new();
        for data_source in &module.data_sources {
            let source_file = module
                .parser_binding_metadata
                .resource_source_file(&data_source.id);
            let expanded = prefix_module_data_source(
                data_source,
                instance_prefix,
                &call.module_name,
                &module_instance_scope,
                &expanded_call,
                &intra_module_bindings,
                argument_substitutions,
            )
            .map_err(|error| {
                contextualize_access_error(error.with_optional_source_file(source_file))
            })?;
            if let Some(source_file) = source_file {
                expanded_parser_metadata
                    .insert_resource_source_file(expanded.id.clone(), source_file.to_path_buf());
            }
            expanded_binding_uses.remap_resource_owner(&data_source.id, &expanded.id);
            data_sources.push(expanded);
        }

        // Propagate the module's own composition resources (synthesized by
        // nested module-call expansion), instance-prefixed.
        let mut compositions: Vec<LiveComposition> = Vec::new();
        for composition in &module.compositions {
            let mut expanded = prefix_module_composition(
                composition,
                instance_prefix,
                &intra_module_bindings,
                argument_substitutions,
                &expanded_call,
                self.config,
                resolved_bindings,
            )
            .map_err(contextualize_access_error)?;
            settle_prefixed_composition_constraints(&mut expanded)?;
            compositions.push(expanded);
        }

        // Every call becomes a composition, including calls with no outputs
        // and anonymous calls. Besides exposing outputs for bound calls, the
        // node is the post-expansion record of the call boundary itself.
        {
            let mut composition_attrs: IndexMap<String, crate::resource::LiveCompositionAttribute> =
                IndexMap::new();

            // Copy attribute values from the module definition.
            // Each value is classified into `Forwarded` (single-hop
            // alias) vs `Derived` (multi-source expression) via
            // `LiveCompositionAttribute::from_value` (#3294).
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    let source_argument = source_argument_name(value, &argument_values);
                    let declared_type = attr_param.type_expr.clone().or_else(|| {
                        source_argument.as_deref().and_then(|source_argument| {
                            module
                                .arguments
                                .iter()
                                .find(|argument| argument.name == source_argument)
                                .map(|argument| argument.type_expr.clone())
                        })
                    });
                    // Rewrite intra-module refs and substitute arguments
                    let rewritten =
                        rewrite_intra_module_refs(value, instance_prefix, &intra_module_bindings);
                    let mut substituted = substitute_arguments_with_position(
                        &rewritten,
                        argument_substitutions,
                        ArgumentSubstitutionPosition::Value,
                    )
                    .map_err(|error| {
                        contextualize_access_error(error.with_source_file(attr_param.source_file()))
                    })?;
                    // Same post-substitution canonicalize as the
                    // resource-attribute path above (#2815 / #2817).
                    substituted.canonicalize_in_place();
                    composition_attrs.insert(
                        attr_param.name.clone(),
                        crate::resource::LiveCompositionAttribute::from_value(
                            substituted,
                            declared_type,
                        )
                        .with_source_argument(source_argument),
                    );
                }
            }

            // Preserve the resolved call-site arguments on the
            // expanded composition itself (#3292). We record them on the
            // composition in `module.arguments` declaration order so
            // the trace is stable across runs, preserving whether each
            // call-site argument was a value or a structural handle.
            let mut signature_arguments: IndexMap<
                String,
                crate::resource::LiveCompositionArgument,
            > = IndexMap::new();
            for arg in &module.arguments {
                if let Some(argument) = resolved_arguments.get(&arg.name) {
                    let recorded = match argument.value_ref() {
                        StructuralOperandRef::Value(value) => {
                            crate::resource::LiveCompositionArgument::from_value(
                                value.clone(),
                                arg.type_expr.clone(),
                                argument.origin,
                            )
                        }
                        StructuralOperandRef::Handle {
                            handle,
                            access_base,
                        } => crate::resource::LiveCompositionArgument::from_handle(
                            access_base.clone(),
                            arg.type_expr.clone(),
                            handle.clone(),
                            argument.origin,
                        ),
                        StructuralOperandRef::Parameter { access_base, .. } => {
                            crate::resource::LiveCompositionArgument::from_value(
                                access_base.clone(),
                                arg.type_expr.clone(),
                                argument.origin,
                            )
                        }
                    };
                    signature_arguments.insert(arg.name.clone(), recorded);
                }
            }

            let composition = LiveComposition {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "_virtual",
                    crate::resource::ResourceIdentity::new(instance_prefix.to_string()),
                )),
                signature: crate::resource::LiveCompositionSignature {
                    arguments: signature_arguments,
                    attributes: composition_attrs,
                    pending_constraints,
                },
                binding: call.binding_name.clone(),
                dependency_bindings: BTreeSet::new(),
                module_name: call.module_name.clone(),
                instance: instance_prefix.to_string(),
                call_path: CompositionCallPath::direct(expanded_call.clone()),
                quoted_string_attrs: HashSet::new(),
            };
            compositions.push(composition);
        }

        // Propagate the module's `wait` declarations, instance-prefixed
        // (see `prefix_wait_binding`), so a module-internal `wait`
        // reaches the caller's plan (carina#3061).
        let expanded_wait_bindings: Vec<WaitBinding> = module
            .wait_bindings
            .iter()
            .map(|wb| prefix_wait_binding(wb, instance_prefix))
            .collect();

        // carina#3126: propagate the module's deferred for-expressions
        // (previously dropped at this boundary). Each entry is
        // instance-prefixed the same way `module.resources` are, so
        // the loop resolves against the prefixed module bindings and
        // its generated resources are isolated per module instance
        // (carina#3126 PR-B).
        let expanded_deferred_for_expressions: Vec<DeferredForExpression> = module
            .deferred_for_expressions
            .iter()
            .map(|d| {
                let source_file = module
                    .parser_binding_metadata
                    .resource_source_file(&d.template_resource.id);
                prefix_deferred_for_expression(
                    d,
                    instance_prefix,
                    &call.module_name,
                    &module_instance_scope,
                    &expanded_call,
                    &intra_module_bindings,
                    argument_substitutions,
                )
                .map_err(|error| error.with_optional_source_file(source_file))
            })
            .collect::<Result<_, _>>()
            .map_err(contextualize_access_error)?;

        // carina#3181: managed resources, data sources, and composition
        // resources are collected into separate typed `Vec`s above —
        // `resources` from `module.resources`, `data_sources`
        // from `module.data_sources`, and `compositions` from
        // `module.compositions` plus the `_virtual` attribute
        // resource built above.

        // #3306: build the expansion trace from this call's leaves +
        // nested-expansion traces inherited from `module`, *before*
        // the struct literal moves `resources` / `data_sources`.
        // carina#3322: stamp the call site with the DSL `use` path
        // so plan rendering can label the group with a DSL-visible
        // name (`module "<binding>" (<source_path>)`). `None` falls
        // back to a path-less header — only relevant in test harnesses
        // that bypass `process_imports`; real expansions always have
        // a recorded path.
        let source_path = module_source.as_deref();
        let expansion_trace = build_expansion_trace(
            instance_prefix,
            source_path,
            &module.expansion_trace,
            &resources,
            &data_sources,
        );

        // A module instance contributes only its instance-prefixed runtime
        // graph. Module-local declarations and boundary inputs remain empty
        // on `ParsedFile::default()` because they were consumed above or are
        // rejected while loading the module.
        let mut contribution = ParsedFile::empty_with_parsed_binding_uses(expanded_binding_uses);
        contribution.resources = resources;
        contribution.data_sources = data_sources;
        contribution.compositions = compositions;
        contribution.wait_bindings = expanded_wait_bindings;
        contribution.deferred_for_expressions = expanded_deferred_for_expressions;
        contribution.parser_binding_metadata = expanded_parser_metadata;
        contribution.warnings = module.warnings.clone();
        contribution.expansion_trace = expansion_trace;
        Ok(contribution)
    }

    /// Unit tests exercise isolated expansion mechanics with synthetic module
    /// maps. Production has no witness-free entry point.
    #[cfg(test)]
    pub(super) fn expand_module_call(
        &self,
        call: &ModuleCall,
        instance_prefix: &str,
    ) -> Result<ParsedFile, ModuleError> {
        self.expand_module_call_checked(
            call,
            instance_prefix,
            &super::resolver::RefCheckWitness(()),
            None,
            None,
        )
    }
}

/// Return the module parameter directly forwarded by an output expression.
///
/// Current parser output uses the typed `ModuleArgument` placeholder; the
/// BindingRef/ResourceRef arms keep constructed and pre-expansion values on
/// the same metadata path. One helper owns the classification so declared
/// output types and duplicate-diagnostic suppression cannot drift by shape.
fn source_argument_name(value: &Value, argument_values: &HashMap<String, Value>) -> Option<String> {
    let candidate = crate::resource::module_argument_reference_path(value)
        .map(|path| path.as_ref().binding().to_string())
        .or_else(|| match value {
            Value::Deferred(DeferredValue::BindingRef { binding }) => Some(binding.clone()),
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                Some(path.binding().to_string())
            }
            _ => None,
        })?;
    argument_values
        .contains_key(&candidate)
        .then_some(candidate)
}

/// Build an [`ExpansionTrace`](crate::resource::ExpansionTrace) for one
/// `expand_module_call` invocation.
///
/// Records the call site (derived from `instance_prefix`) against
/// every leaf produced at this level, and prepends the same call site
/// to every chain inherited from the module's own
/// `expansion_trace` (so nested expansions surface a full outermost-
/// first chain in the merged plan).
pub(crate) fn build_expansion_trace(
    instance_prefix: &str,
    source_path: Option<&str>,
    inner_trace: &crate::resource::ExpansionTrace,
    leaf_resources: &[Resource],
    leaf_data_sources: &[DataSource],
) -> crate::resource::ExpansionTrace {
    let id = crate::resource::EphemeralId::new(crate::resource::ResolvedResourceId::new(
        crate::resource::ResourceId::with_identity(
            "_virtual",
            crate::resource::ResourceIdentity::new(instance_prefix.to_string()),
        ),
    ));
    let this_call_site = match source_path {
        Some(p) => crate::resource::CallSite::new(id, p),
        None => crate::resource::CallSite::without_source(id),
    };

    let mut out = crate::resource::ExpansionTrace::new();

    // Direct leaves at this expansion level: chain is `[this_call_site]`.
    for r in leaf_resources {
        out.record(r.persistent_id(), vec![this_call_site.clone()]);
    }
    for d in leaf_data_sources {
        out.record(d.persistent_id(), vec![this_call_site.clone()]);
    }

    // Leaves from nested expansions inside `module` already have an
    // inner chain; prepend the current call site to mark this level as
    // their outer-most container.
    for (leaf, inner_chain) in inner_trace.iter() {
        let mut chain = Vec::with_capacity(inner_chain.len() + 1);
        chain.push(this_call_site.clone());
        chain.extend(inner_chain.iter().cloned());
        out.record(leaf.clone(), chain);
    }

    out
}

/// Join a module-call `instance_prefix` to an intra-module binding name
/// (`<prefix>.<name>`, dot-separated instance path).
///
/// This is the *single* definition of the instance-prefix spelling.
/// Every site that prefixes a binding — resource ids, resource
/// bindings, dependency bindings, `rewrite_intra_module_refs`, and
/// [`prefix_wait_binding`] — routes through here so the format can never
/// drift between binding kinds (the carina#3061 class of bug was a binding
/// kind that was *not* prefixed at all; keeping one spelling makes "is this
/// kind prefixed?" a single, greppable call site per kind).
fn apply_instance_prefix(instance_prefix: &str, name: &str) -> String {
    format!("{instance_prefix}.{name}")
}

/// Prefix dependency metadata that names bindings owned by this module.
///
/// Argument names and caller/outer-scope bindings are deliberately left
/// unchanged: they are not members of `intra_module_bindings`, matching the
/// discrimination used by [`rewrite_intra_module_refs`] for value-shaped
/// references.
fn prefix_directive_dependency(
    binding: &str,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<String, ArgumentSubstitutionError> {
    if let Some(argument) = arguments.arguments.get(binding) {
        return match argument.value_ref() {
            StructuralOperandRef::Handle { handle, .. } => {
                if handle.kind().is_dependency_target() {
                    Ok(handle.canonical_binding_name().to_string())
                } else {
                    Err(ArgumentSubstitutionError {
                        source_file: Some(arguments.caller_source_file.to_path_buf()),
                        line: argument.source_line(arguments.default_line),
                        message: format!(
                            "directives.depends_on: '{}' is {} and cannot be used as a dependency target",
                            handle
                                .alias_name()
                                .unwrap_or_else(|| handle.canonical_binding_name()),
                            handle.kind().description(),
                        ),
                        parse_error: None,
                    })
                }
            }
            StructuralOperandRef::Value(_) => Err(ArgumentSubstitutionError {
                source_file: Some(arguments.caller_source_file.to_path_buf()),
                line: argument.source_line(arguments.default_line),
                message: format!(
                    "directives.depends_on parameter '{binding}' received a value, not a resource handle"
                ),
                parse_error: None,
            }),
            StructuralOperandRef::Parameter { name, .. } => Ok(name.to_string()),
        };
    }
    if intra_module_bindings.contains(binding) {
        Ok(apply_instance_prefix(instance_prefix, binding))
    } else {
        Ok(binding.to_string())
    }
}

fn prefix_dependency_bindings(
    dependency_bindings: &BTreeSet<String>,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<BTreeSet<String>, ArgumentSubstitutionError> {
    let mut rewritten = BTreeSet::new();
    for binding in dependency_bindings {
        if let Some(argument) = arguments.arguments.get(binding) {
            match argument.value_ref() {
                StructuralOperandRef::Handle { handle, .. } => {
                    rewritten.insert(handle.canonical_binding_name().to_string());
                }
                StructuralOperandRef::Value(value) => {
                    let mut dependencies = HashSet::new();
                    crate::deps::collect_dependencies(value, &mut dependencies);
                    rewritten.extend(dependencies);
                }
                StructuralOperandRef::Parameter { name, .. } => {
                    rewritten.insert(name.to_string());
                }
            }
        } else if intra_module_bindings.contains(binding) {
            rewritten.insert(apply_instance_prefix(instance_prefix, binding));
        } else {
            rewritten.insert(binding.clone());
        }
    }
    Ok(rewritten)
}

fn prefix_directive_dependencies(
    dependencies: &[String],
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<Vec<String>, ArgumentSubstitutionError> {
    dependencies
        .iter()
        .map(|binding| {
            prefix_directive_dependency(binding, instance_prefix, intra_module_bindings, arguments)
        })
        .collect()
}

/// `BindingName`-typed instance-prefix: the wait path's binding-name
/// fields are `BindingName`, so prefixing them is a typed
/// `BindingName -> BindingName` transition (a raw `String` can't slip
/// into a binding-name position). Delegates to [`apply_instance_prefix`]
/// for the single spelling.
fn prefix_binding_name(instance_prefix: &str, name: &BindingName) -> BindingName {
    BindingName::new(apply_instance_prefix(instance_prefix, name.as_str()))
}

/// Instance-prefix every binding-name field of a [`WaitBinding`] so a
/// module-internal `wait` keeps referring to the same (now prefixed)
/// target / dependencies after expansion.
///
/// The `WaitBinding` is **destructured exhaustively** rather than
/// field-accessed: if a future field is added, this stops compiling
/// until someone decides whether that field is a binding name (prefix
/// it) or value/provenance (pass through). That compile-time forcing
/// function is the carina#3061 guard — the original bug was a
/// binding-carrying structure whose propagation silently skipped a
/// part. Today: `binding`, `target`, the predicate LHS root segment
/// (`lhs_segments[0]`, pinned to the target binding by
/// `parse_wait_expr`), and every `depends_on` entry are prefixed;
/// `until_raw` (verbatim user surface text), `until_predicate.rhs` (a
/// comparison value), `timeout_secs`, and `line` are not binding names
/// and pass through unchanged.
fn prefix_wait_binding(wb: &WaitBinding, instance_prefix: &str) -> WaitBinding {
    let WaitBinding {
        binding,
        target,
        until_raw,
        until_predicate,
        timeout_secs,
        depends_on,
        line,
    } = wb;
    let prefixed_name = |n: &BindingName| prefix_binding_name(instance_prefix, n);

    let mut until_predicate = until_predicate.clone();
    // `lhs_segments[0]` is the (string) target-binding segment of a
    // mixed path `[target, attr, ...]`; only that head is a binding
    // name, so it takes the string-level prefix spelling, not the
    // `BindingName` wrapper (the rest are attribute path segments).
    if let Some(root) = until_predicate.lhs_segments.first_mut() {
        *root = apply_instance_prefix(instance_prefix, root);
    }

    WaitBinding {
        binding: prefixed_name(binding),
        target: prefixed_name(target),
        until_raw: until_raw.clone(),
        until_predicate,
        timeout_secs: *timeout_secs,
        depends_on: depends_on.iter().map(prefixed_name).collect(),
        line: *line,
    }
}

#[derive(Clone)]
struct ResolvedArgument {
    argument: ModuleArgument,
    origin: ArgumentOrigin,
}

impl std::ops::Deref for ResolvedArgument {
    type Target = ModuleArgument;

    fn deref(&self) -> &Self::Target {
        &self.argument
    }
}

impl ResolvedArgument {
    fn source_line(&self, default_line: usize) -> usize {
        match self.origin {
            ArgumentOrigin::Authored { value_span, .. } => value_span.start_line(),
            ArgumentOrigin::Defaulted => default_line,
        }
    }

    fn substitution(
        &self,
        name: &str,
        position: ArgumentSubstitutionPosition,
    ) -> Result<StructuralOperandRef<'_>, String> {
        match (position, self.value_ref()) {
            (ArgumentSubstitutionPosition::Value, StructuralOperandRef::Value(value)) => {
                Ok(StructuralOperandRef::Value(value))
            }
            (ArgumentSubstitutionPosition::Value, StructuralOperandRef::Handle { .. }) => {
                Err(BareStructuralHandle {
                    binding: name.to_string(),
                    kind: StructuralHandleKind::ModuleParameter,
                }
                .to_string())
            }
            (
                ArgumentSubstitutionPosition::Value,
                operand @ StructuralOperandRef::Parameter { .. },
            ) => Ok(operand),
            (ArgumentSubstitutionPosition::AccessBase, _) => Ok(self.access_ref()),
        }
    }
}

#[derive(Clone, Copy)]
struct ArgumentSubstitutions<'a> {
    arguments: &'a HashMap<String, ResolvedArgument>,
    default_line: usize,
    caller_source_file: &'a std::path::Path,
    binding_index: Option<&'a BindingIndex<'a>>,
    config: &'a crate::parser::ProviderContext,
}

#[derive(Clone, Copy)]
enum ArgumentSubstitutionPosition {
    Value,
    AccessBase,
}

fn bind_module_constraints(
    module: &ParsedFile,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
) -> Result<Vec<crate::resource::PendingModuleConstraint>, ArgumentSubstitutionError> {
    let mut constraints = Vec::new();
    for parameter in &module.arguments {
        for (ordinal, validation) in parameter.validations.iter().enumerate() {
            let line = validation.source_span().start_line();
            let source_file = validation.source_file();
            constraints.push(crate::resource::PendingModuleConstraint {
                id: crate::resource::ModuleConstraintId::argument_validation(
                    &parameter.name,
                    ordinal,
                ),
                expression: bind_constraint_expression(
                    &validation.condition,
                    arguments,
                    resolved_bindings,
                    line,
                    source_file,
                )?,
                message: validation.error_message.clone().unwrap_or_else(|| {
                    format!("validation failed for argument '{}'", parameter.name)
                }),
            });
        }
    }
    for (ordinal, requirement) in module.requires.iter().enumerate() {
        let line = requirement.source_span().start_line();
        let source_file = requirement.source_file();
        constraints.push(crate::resource::PendingModuleConstraint {
            id: crate::resource::ModuleConstraintId::require(ordinal),
            expression: bind_constraint_expression(
                &requirement.condition,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?,
            message: requirement.error_message.clone(),
        });
    }
    Ok(constraints)
}

fn bind_constraint_expression(
    expression: &crate::parser::ValidateExpr,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
    line: usize,
    source_file: &std::path::Path,
) -> Result<crate::parser::ValidateExpr, ArgumentSubstitutionError> {
    use crate::parser::ValidateExpr;

    let bind_value = |argument: &str,
                      position: ArgumentSubstitutionPosition,
                      path: &[PathSegment]|
     -> Result<ValidateExpr, ArgumentSubstitutionError> {
        let Some(resolved_argument) = arguments.arguments.get(argument) else {
            return Ok(expression.clone());
        };
        let substitution =
            resolved_argument
                .substitution(argument, position)
                .map_err(|message| ArgumentSubstitutionError {
                    source_file: Some(source_file.to_path_buf()),
                    line,
                    message,
                    parse_error: None,
                })?;
        let mut value = substitution.value().clone();
        for step in path {
            value = apply_value_path_segment(value, step.clone(), line, AccessMissPolicy::Error)
                .and_then(ValuePathSegmentResult::into_value)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: Some(source_file.to_path_buf()),
                    line,
                    message: error.to_string(),
                    parse_error: None,
                })?;
        }
        if let Some(bindings) = resolved_bindings
            && let Ok(resolved) = crate::resolver::resolve_ref_value(&value, bindings)
        {
            value = resolved;
        }
        Ok(ValidateExpr::BoundArgument {
            argument: argument.to_string(),
            operand: path.iter().fold(argument.to_string(), |mut operand, step| {
                operand.push_str(&step.to_dot_string());
                operand
            }),
            value,
        })
    };

    Ok(match expression {
        ValidateExpr::Var(argument) => {
            return bind_value(argument, ArgumentSubstitutionPosition::Value, &[]);
        }
        ValidateExpr::Access { argument, path } => {
            return bind_value(argument, ArgumentSubstitutionPosition::AccessBase, path);
        }
        ValidateExpr::BoundArgument { .. }
        | ValidateExpr::Bool(_)
        | ValidateExpr::Int(_)
        | ValidateExpr::Float(_)
        | ValidateExpr::Duration(_)
        | ValidateExpr::String(_)
        | ValidateExpr::Null => expression.clone(),
        ValidateExpr::Compare { lhs, op, rhs } => ValidateExpr::Compare {
            lhs: Box::new(bind_constraint_expression(
                lhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
            op: op.clone(),
            rhs: Box::new(bind_constraint_expression(
                rhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
        },
        ValidateExpr::And(lhs, rhs) => ValidateExpr::And(
            Box::new(bind_constraint_expression(
                lhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
            Box::new(bind_constraint_expression(
                rhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
        ),
        ValidateExpr::Or(lhs, rhs) => ValidateExpr::Or(
            Box::new(bind_constraint_expression(
                lhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
            Box::new(bind_constraint_expression(
                rhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
        ),
        ValidateExpr::Not(inner) => ValidateExpr::Not(Box::new(bind_constraint_expression(
            inner,
            arguments,
            resolved_bindings,
            line,
            source_file,
        )?)),
        ValidateExpr::FunctionCall { name, args } => ValidateExpr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|argument| {
                    bind_constraint_expression(
                        argument,
                        arguments,
                        resolved_bindings,
                        line,
                        source_file,
                    )
                })
                .collect::<Result<_, _>>()?,
        },
    })
}

/// Rewrite values already bound into a nested module constraint while that
/// composition crosses its next module boundary. This is the constraint
/// counterpart of [`prefix_attr_value`]: local resource references gain the
/// outer instance prefix, and accesses rooted at an outer module parameter
/// are substituted from the same typed argument map as resource attributes.
/// The constraint's argument name remains local to the nested module so
/// diagnostics and actual-value reporting retain the authored boundary.
fn prefix_constraint_expression(
    expression: &crate::parser::ValidateExpr,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
) -> Result<crate::parser::ValidateExpr, ArgumentSubstitutionError> {
    use crate::parser::ValidateExpr;

    Ok(match expression {
        ValidateExpr::BoundArgument {
            argument,
            operand,
            value,
        } => {
            let mut value =
                prefix_attr_value(value, instance_prefix, intra_module_bindings, arguments)?;
            if let Some(bindings) = resolved_bindings
                && let Ok(resolved) = crate::resolver::resolve_ref_value(&value, bindings)
            {
                value = resolved;
            }
            ValidateExpr::BoundArgument {
                argument: argument.clone(),
                operand: operand.clone(),
                value,
            }
        }
        ValidateExpr::Compare { lhs, op, rhs } => ValidateExpr::Compare {
            lhs: Box::new(prefix_constraint_expression(
                lhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
            op: op.clone(),
            rhs: Box::new(prefix_constraint_expression(
                rhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
        },
        ValidateExpr::And(lhs, rhs) => ValidateExpr::And(
            Box::new(prefix_constraint_expression(
                lhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
            Box::new(prefix_constraint_expression(
                rhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
        ),
        ValidateExpr::Or(lhs, rhs) => ValidateExpr::Or(
            Box::new(prefix_constraint_expression(
                lhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
            Box::new(prefix_constraint_expression(
                rhs,
                instance_prefix,
                intra_module_bindings,
                arguments,
                resolved_bindings,
            )?),
        ),
        ValidateExpr::Not(inner) => ValidateExpr::Not(Box::new(prefix_constraint_expression(
            inner,
            instance_prefix,
            intra_module_bindings,
            arguments,
            resolved_bindings,
        )?)),
        ValidateExpr::FunctionCall { name, args } => ValidateExpr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|argument| {
                    prefix_constraint_expression(
                        argument,
                        instance_prefix,
                        intra_module_bindings,
                        arguments,
                        resolved_bindings,
                    )
                })
                .collect::<Result<_, _>>()?,
        },
        ValidateExpr::Var(_)
        | ValidateExpr::Access { .. }
        | ValidateExpr::Bool(_)
        | ValidateExpr::Int(_)
        | ValidateExpr::Float(_)
        | ValidateExpr::Duration(_)
        | ValidateExpr::String(_)
        | ValidateExpr::Null => expression.clone(),
    })
}

/// Expand one attribute value crossing the module boundary:
/// `rewrite_intra_module_refs` (prefix intra-module refs; a
/// caller-provided ref that coincidentally shares a binding name is
/// left alone) → `substitute_arguments` → `canonicalize_in_place`
/// (collapse a fully-literal `Interpolation` back to a flat `String`,
/// #2815 / #2817).
///
/// The single definition of "expand a module attribute value", shared
/// by [`prefix_module_resource`] (a module resource's attrs) and
/// [`prefix_deferred_for_expression`] (a loop body's attrs) so the two
/// carriers can never drift — the exact carina#3126 bug class.
fn prefix_attr_value(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<Value, ArgumentSubstitutionError> {
    let rewritten = rewrite_intra_module_refs(value, instance_prefix, intra_module_bindings);
    let mut substituted = substitute_arguments_with_position(
        &rewritten,
        arguments,
        ArgumentSubstitutionPosition::Value,
    )?;
    substituted.canonicalize_in_place();
    Ok(substituted)
}

/// Instance-prefix one module resource: `Bound` id name, binding, and
/// intra-module dependency metadata take the instance prefix, typed
/// module-source is stamped, and every attribute is
/// `rewrite_intra_module_refs` → `substitute_arguments` →
/// `canonicalize_in_place` (the #2222 / #2815 / #2817 sequence).
///
/// The single definition of "expand a module resource", shared by the
/// `module.resources` loop and `prefix_deferred_for_expression`'s
/// `template_resource` so a loop body inside a module is prefixed
/// identically to a top-level module resource (carina#3126 PR-B).
fn prefix_module_resource(
    resource: &Resource,
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<Resource, ArgumentSubstitutionError> {
    let instance_identity = ResourceIdentity::try_from(instance_prefix.to_string())
        .expect("module instance prefix is non-empty");
    prefix_module_resource_with_identity(
        resource,
        instance_prefix,
        &instance_identity,
        module_name,
        module_instance_scope,
        module_call,
        intra_module_bindings,
        arguments,
    )
}

#[allow(clippy::too_many_arguments)]
fn prefix_module_resource_with_identity(
    resource: &Resource,
    instance_prefix: &str,
    instance_identity: &ResourceIdentity,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<Resource, ArgumentSubstitutionError> {
    // Only assigned identities take the prefix here. Anonymous resources
    // stay pending so `compute_anonymous_identifiers` can later attach both
    // the hash and the instance prefix in one shot (#2516). Each module
    // instance receives a distinct pending token.
    let mut new_resource = match resource.id.identity_state() {
        ResourceIdentityState::Pending(_) => resource.instantiate(),
        ResourceIdentityState::Resolved(identity) => resource.instantiate_with_identity(
            ResourceIdentity::new(apply_instance_prefix(instance_prefix, identity.as_str())),
        ),
    };

    let legacy_parent = module_call
        .loop_origin()
        .map(|origin| origin.legacy_address().clone())
        .unwrap_or_else(|| LegacyLoopAddress::new(instance_identity.as_str()));
    new_resource
        .loop_provenance
        .prefix_for_module(instance_identity, &legacy_parent);
    if let Some(origin) = module_call.loop_origin() {
        new_resource.loop_provenance.push_module_prefix(
            crate::resource::LoopModulePrefixOrigin::new(origin.clone(), instance_identity.clone()),
        );
    }

    if let Some(ref binding) = new_resource.binding {
        new_resource.binding = Some(apply_instance_prefix(instance_prefix, binding));
    }
    new_resource.directives.depends_on = prefix_directive_dependencies(
        &new_resource.directives.depends_on,
        instance_prefix,
        intra_module_bindings,
        arguments,
    )?;
    new_resource.dependency_bindings = prefix_dependency_bindings(
        &new_resource.dependency_bindings,
        instance_prefix,
        intra_module_bindings,
        arguments,
    )?;

    let (scope, call_path) = match new_resource.module_source.as_ref() {
        Some(ModuleSource::Module(child)) => (
            module_instance_scope.with_child(child.identity_scope()),
            child
                .module_call_path()
                .clone()
                .prepend(module_call.clone()),
        ),
        Some(ModuleSource::Root) | None => (
            module_instance_scope.clone(),
            CompositionCallPath::direct(module_call.clone()),
        ),
    };
    new_resource.module_source = Some(ModuleSource::expanded_module(
        module_name,
        instance_prefix,
        scope,
        call_path,
    ));

    // `IndexMap` preserves authored attribute order across expansion
    // (#2222); each value goes through the shared `prefix_attr_value`.
    let mut substituted_attrs: IndexMap<String, Value> = IndexMap::new();
    for (key, expr) in &new_resource.attributes {
        substituted_attrs.insert(
            key.clone(),
            prefix_attr_value(expr, instance_prefix, intra_module_bindings, arguments)?,
        );
    }
    new_resource.attributes = substituted_attrs;

    Ok(new_resource)
}

/// Instance-prefix one module data source — the [`DataSource`] analogue
/// of [`prefix_module_resource`]. A `DataSource` carries no `prefixes`
/// field, so its assigned identity, `binding`, intra-module dependency
/// metadata, `module_source`, and attributes take the module-boundary
/// treatment.
fn prefix_module_data_source(
    data_source: &DataSource,
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<DataSource, ArgumentSubstitutionError> {
    let mut new_data_source = match data_source.id.identity_state() {
        ResourceIdentityState::Pending(_) => data_source.instantiate(),
        ResourceIdentityState::Resolved(identity) => data_source.instantiate_with_identity(
            ResourceIdentity::new(apply_instance_prefix(instance_prefix, identity.as_str())),
        ),
    };

    if let Some(ref binding) = new_data_source.binding {
        new_data_source.binding = Some(apply_instance_prefix(instance_prefix, binding));
    }
    new_data_source.directives.depends_on = prefix_directive_dependencies(
        &new_data_source.directives.depends_on,
        instance_prefix,
        intra_module_bindings,
        arguments,
    )?;
    new_data_source.dependency_bindings = prefix_dependency_bindings(
        &new_data_source.dependency_bindings,
        instance_prefix,
        intra_module_bindings,
        arguments,
    )?;

    let (scope, call_path) = match new_data_source.module_source.as_ref() {
        Some(ModuleSource::Module(child)) => (
            module_instance_scope.with_child(child.identity_scope()),
            child
                .module_call_path()
                .clone()
                .prepend(module_call.clone()),
        ),
        Some(ModuleSource::Root) | None => (
            module_instance_scope.clone(),
            CompositionCallPath::direct(module_call.clone()),
        ),
    };
    new_data_source.module_source = Some(ModuleSource::expanded_module(
        module_name,
        instance_prefix,
        scope,
        call_path,
    ));

    let mut substituted_attrs: IndexMap<String, Value> = IndexMap::new();
    for (key, expr) in &new_data_source.attributes {
        substituted_attrs.insert(
            key.clone(),
            prefix_attr_value(expr, instance_prefix, intra_module_bindings, arguments)?,
        );
    }
    new_data_source.attributes = substituted_attrs;

    Ok(new_data_source)
}

/// Instance-prefix one composition resource crossing a module boundary — the
/// [`LiveComposition`] analogue of [`prefix_module_resource`]. The flattened
/// identity/binding, diagnostic call provenance, arguments, and attributes all
/// take the prefix treatment; compositions have no `prefixes` or `directives`.
fn prefix_module_composition(
    composition: &LiveComposition,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
    root_call: &CompositionCall,
    config: &crate::parser::ProviderContext,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings>,
) -> Result<LiveComposition, ArgumentSubstitutionError> {
    let mut new_virtual = composition.clone();

    for call in new_virtual.call_path.calls_mut() {
        call.instance = apply_instance_prefix(instance_prefix, &call.instance);
        if let Some(binding) = &call.binding {
            call.binding = Some(apply_instance_prefix(instance_prefix, binding));
        }
    }
    new_virtual.call_path = new_virtual.call_path.clone().prepend(root_call.clone());

    let new_name = apply_instance_prefix(instance_prefix, composition.id.identity_str());
    new_virtual.id = composition
        .id
        .instantiate_with_identity(ResourceIdentity::new(new_name));

    if let Some(ref binding) = new_virtual.binding {
        new_virtual.binding = Some(apply_instance_prefix(instance_prefix, binding));
    }
    new_virtual.instance = apply_instance_prefix(instance_prefix, &new_virtual.instance);

    let mut substituted_arguments: IndexMap<String, crate::resource::LiveCompositionArgument> =
        IndexMap::new();
    for (key, argument) in &new_virtual.signature.arguments {
        if let Some(parameter) = argument.forwarded_parameter()
            && let Some(resolved_parameter) = arguments.arguments.get(parameter)
        {
            let source_line = resolved_parameter.source_line(arguments.default_line);
            let substitution = resolved_parameter
                .substitution(parameter, ArgumentSubstitutionPosition::AccessBase)
                .map_err(|message| ArgumentSubstitutionError {
                    source_file: None,
                    line: source_line,
                    message,
                    parse_error: None,
                })?;
            let mut value = substitution.value().clone();
            value.canonicalize_in_place();
            let resolved = match substitution {
                StructuralOperandRef::Handle { handle, .. } => {
                    argument.with_handle(value, handle.clone().with_alias(parameter))
                }
                StructuralOperandRef::Value(_) | StructuralOperandRef::Parameter { .. } => {
                    argument.with_value(value)
                }
            };
            check_composition_argument_type(key, &resolved, config, source_line)?;
            substituted_arguments.insert(key.clone(), resolved);
            continue;
        }
        let rewritten =
            rewrite_intra_module_refs(argument.value(), instance_prefix, intra_module_bindings);
        let position = if argument.is_handle() {
            ArgumentSubstitutionPosition::AccessBase
        } else {
            ArgumentSubstitutionPosition::Value
        };
        let mut prefixed = substitute_arguments_with_position(&rewritten, arguments, position)?;
        prefixed.canonicalize_in_place();
        let resolved = argument.with_value(prefixed);
        check_composition_argument_type(key, &resolved, config, arguments.default_line)?;
        substituted_arguments.insert(key.clone(), resolved);
    }
    new_virtual.signature.arguments = substituted_arguments;

    let mut substituted_attrs: IndexMap<String, crate::resource::LiveCompositionAttribute> =
        IndexMap::new();
    for (key, attr) in &new_virtual.signature.attributes {
        // Round-trip through `Value` so the existing `prefix_attr_value`
        // helper (which knows about every nested `Value` shape) can
        // run unmodified, then re-classify the result. A bare
        // `Forwarded` may transform into another `Forwarded`
        // (same shape, instance-prefixed binding) or stay `Derived`
        // depending on what `prefix_attr_value` did.
        let v = attr.to_value();
        let prefixed = prefix_attr_value(&v, instance_prefix, intra_module_bindings, arguments)?;
        substituted_attrs.insert(key.clone(), attr.with_value(prefixed));
    }
    new_virtual.signature.attributes = substituted_attrs;

    new_virtual.signature.pending_constraints = new_virtual
        .signature
        .pending_constraints
        .iter()
        .map(|constraint| {
            Ok(crate::resource::PendingModuleConstraint {
                id: constraint.id.clone(),
                expression: prefix_constraint_expression(
                    &constraint.expression,
                    instance_prefix,
                    intra_module_bindings,
                    arguments,
                    resolved_bindings,
                )?,
                message: constraint.message.clone(),
            })
        })
        .collect::<Result<_, ArgumentSubstitutionError>>()?;

    Ok(new_virtual)
}

/// Re-evaluate a nested composition after its outer caller has supplied the
/// arguments that were unavailable during the nested module's first
/// expansion. Satisfied constraints disappear, unresolved constraints remain
/// for plan/apply gates, and concrete failures use the same typed diagnostic
/// path as direct module calls.
fn settle_prefixed_composition_constraints(
    composition: &mut LiveComposition,
) -> Result<(), ModuleError> {
    if composition.signature.pending_constraints.is_empty() {
        return Ok(());
    }

    let argument_values = composition
        .signature
        .arguments
        .iter()
        .map(|(name, argument)| (name.clone(), argument.value().clone()))
        .collect::<HashMap<_, _>>();
    let constraints = std::mem::take(&mut composition.signature.pending_constraints);
    let evaluated =
        evaluate_module_constraints(ModuleConstraints::pending(&constraints), &argument_values);
    let mut pending = Vec::new();
    for evaluated in evaluated {
        match evaluated.evaluation() {
            ConstraintEvaluation::Satisfied => {}
            ConstraintEvaluation::Pending => pending.push(evaluated.into_constraint()),
            ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_) => {
                return Err(evaluated
                    .module_error(
                        &composition.module_name,
                        &composition.instance,
                        composition.binding.as_deref(),
                        Some(&composition.call_path.immediate().id),
                    )
                    .expect("concrete nested constraint outcome has a diagnostic"));
            }
        }
    }
    composition.signature.pending_constraints = pending;
    Ok(())
}

fn check_composition_argument_type(
    parameter: &str,
    argument: &crate::resource::LiveCompositionArgument,
    config: &crate::parser::ProviderContext,
    line: usize,
) -> Result<(), ArgumentSubstitutionError> {
    let declared_type = argument.declared_type();
    let Some(resolved_parameter) =
        crate::validation::resolve_parameter_type_for_check(declared_type, config).map_err(
            |message| ArgumentSubstitutionError {
                source_file: None,
                line,
                message,
                parse_error: None,
            },
        )?
    else {
        return Ok(());
    };

    if let Some(handle) = argument.handle() {
        if let Err(error) = crate::validation::check_handle_sink(handle, &resolved_parameter) {
            return Err(ArgumentSubstitutionError {
                source_file: None,
                line,
                message: format!("parameter '{parameter}': {error}"),
                parse_error: None,
            });
        }
        return Ok(());
    }

    crate::validation::typecheck::check_type_expr_value(
        &resolved_parameter.as_type_expr(),
        argument.value(),
        config,
        crate::validation::typecheck::TypecheckInference::Unavailable,
    )
    .map_err(|mismatch| ArgumentSubstitutionError {
        source_file: None,
        line,
        message: format!("parameter '{parameter}': {mismatch}"),
        parse_error: None,
    })
}

/// Instance-prefix a [`DeferredForExpression`] crossing a module
/// boundary, expanding its loop body the same way the
/// `module.resources` expansion ([`prefix_module_resource`]) does.
///
/// The struct is **destructured exhaustively** — the carina#3061 /
/// carina#3126 compile-time forcing function: a new
/// `DeferredForExpression` field stops this compiling until someone
/// classifies it as binding-name (prefix), value/provenance (pass
/// through), or loop-local (pass through).
///
/// carina#3126 PR-B: the loop body declared inside a module must be
/// expanded *as if it were a module resource* — otherwise its
/// generated resources collide across module instances and its
/// iterable resolves against the wrong (unprefixed) binding. Per
/// field:
/// - `binding_name` (the generated-resource address prefix, e.g.
///   `_domain_validation_options`) → instance-prefixed
///   unconditionally, exactly like `Resource.binding`, so each module
///   instance's loop resources are uniquely addressed.
/// - `attributes` and `template_resource` → the full module-resource
///   treatment via [`prefix_module_resource`] / the same
///   ref-rewrite + arg-substitute + canonicalize as `module.resources`.
/// - `file`/`line`/`header`/`resource_type`/`binding`
///   → provenance / display / non-binding / loop-local: pass through.
fn prefix_deferred_for_expression(
    d: &DeferredForExpression,
    instance_prefix: &str,
    module_name: &str,
    module_instance_scope: &ModuleInstanceScope,
    module_call: &CompositionCall,
    intra_module_bindings: &HashSet<String>,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<DeferredForExpression, ArgumentSubstitutionError> {
    let DeferredForExpression {
        file,
        line,
        header,
        resource_type,
        attributes,
        target,
        iterable,
        binding,
        template_resource,
    } = d;
    let instance_identity = ResourceIdentity::try_from(instance_prefix.to_string())
        .expect("module instance prefix is non-empty");
    let legacy_parent = module_call
        .loop_origin()
        .map(|origin| origin.legacy_address().clone())
        .unwrap_or_else(|| LegacyLoopAddress::new(instance_identity.as_str()));

    Ok(DeferredForExpression {
        // provenance — pass through (like WaitBinding.line)
        file: file.clone(),
        line: *line,
        // verbatim user-surface display text — pass through
        // (like WaitBinding.until_raw)
        header: header.clone(),
        // provider-qualified type — not a binding
        resource_type: resource_type.clone(),
        // The loop body's attrs may reference module-internal bindings
        // / module arguments — same shared `prefix_attr_value` the
        // module resource attrs get.
        attributes: attributes
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    prefix_attr_value(v, instance_prefix, intra_module_bindings, arguments)?,
                ))
            })
            .collect::<Result<_, ArgumentSubstitutionError>>()?,
        // Generated-resource address prefix — `binding_name` is a name
        // the loop *synthesizes* for its own resources (e.g.
        // `_domain_validation_options`), NOT a reference to a
        // pre-existing binding, so the caller-collision concern does
        // not apply: prefix it unconditionally (same as
        // `Resource.binding`) to isolate each module instance's loop
        // resources.
        target: match target {
            crate::parser::DeferredForTarget::Bound { binding } => {
                crate::parser::DeferredForTarget::Bound {
                    binding: apply_instance_prefix(instance_prefix, binding),
                }
            }
            crate::parser::DeferredForTarget::Unbound { template } => {
                let mut template = template.clone();
                template.prefix_for_module(&instance_identity, &legacy_parent);
                crate::parser::DeferredForTarget::Unbound { template }
            }
        },
        // The full iterable is a value expression: rewrite every reference
        // and substitute module arguments in both coalesce operands.
        iterable: prefix_attr_value(iterable, instance_prefix, intra_module_bindings, arguments)?,
        // loop-var pattern kind — loop-local, not a module binding
        binding: binding.clone(),
        // Full module-resource treatment — identical to a
        // `module.resources` entry.
        template_resource: prefix_module_resource_with_identity(
            template_resource,
            instance_prefix,
            &instance_identity,
            module_name,
            module_instance_scope,
            module_call,
            intra_module_bindings,
            arguments,
        )?,
    })
}

#[derive(Debug)]
pub(super) struct ArgumentSubstitutionError {
    source_file: Option<std::path::PathBuf>,
    line: usize,
    message: String,
    parse_error: Option<Box<ParseError>>,
}

impl ArgumentSubstitutionError {
    fn with_optional_source_file(self, source_file: Option<&std::path::Path>) -> Self {
        match source_file {
            Some(source_file) => self.with_source_file(source_file),
            None => self,
        }
    }

    fn with_source_file(mut self, source_file: &std::path::Path) -> Self {
        if self.source_file.is_none() {
            self.source_file = Some(source_file.to_path_buf());
        }
        if let Some(error) = self.parse_error.take() {
            let error = *error;
            self.parse_error = Some(Box::new(if error.source_file().is_some() {
                error
            } else {
                error.in_file(source_file.to_path_buf())
            }));
        }
        self
    }
}

/// Substitute arguments references with actual values.
///
/// Argument parameter names are registered as typed `ModuleArgument`
/// placeholders in the parser. Postfix access stays an
/// [`Value::Deferred(DeferredValue::Access)`] rooted at that placeholder until
/// caller values are substituted. BindingRef/ResourceRef substitution remains
/// for pre-expanded or hand-constructed AST values.
#[cfg(test)]
pub(super) fn substitute_arguments(
    value: &Value,
    arguments: &HashMap<String, Value>,
) -> Result<Value, ArgumentSubstitutionError> {
    let config = crate::parser::ProviderContext::default();
    let arguments = arguments
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                ResolvedArgument {
                    argument: ModuleArgument::new(value.clone()),
                    origin: ArgumentOrigin::Defaulted,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    substitute_arguments_with_position(
        value,
        ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        },
        ArgumentSubstitutionPosition::Value,
    )
}

fn function_substitution_error(
    error: ParseError,
    fallback_line: usize,
) -> ArgumentSubstitutionError {
    ArgumentSubstitutionError {
        source_file: error.source_file().map(std::path::Path::to_path_buf),
        line: fallback_line,
        message: error.to_string(),
        parse_error: Some(Box::new(error)),
    }
}

fn substitute_function_argument(
    argument: &crate::resource::FunctionArgument,
    arguments: ArgumentSubstitutions<'_>,
) -> Result<crate::resource::FunctionArgument, ArgumentSubstitutionError> {
    if let Some(name) = argument.pending_parameter()
        && let Some(substitution) = arguments.arguments.get(name)
    {
        return match substitution.value_ref() {
            StructuralOperandRef::Handle { handle, .. } => Ok(
                crate::resource::FunctionArgument::from_handle(handle.clone()),
            ),
            StructuralOperandRef::Value(value) => {
                Ok(crate::resource::FunctionArgument::from_value(value.clone()))
            }
            StructuralOperandRef::Parameter { .. } => substitute_arguments_with_position(
                argument.walker_value(),
                arguments,
                ArgumentSubstitutionPosition::Value,
            )
            .map(crate::resource::FunctionArgument::from_value),
        };
    }

    argument.try_map_value(|value| {
        substitute_arguments_with_position(value, arguments, ArgumentSubstitutionPosition::Value)
    })
}

fn substitute_arguments_with_position(
    value: &Value,
    arguments: ArgumentSubstitutions<'_>,
    position: ArgumentSubstitutionPosition,
) -> Result<Value, ArgumentSubstitutionError> {
    let substituted = match value {
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::UnknownReason::ModuleArgument {
                name,
                source_line,
                diagnostic_name,
            },
        )) => {
            let Some(argument) = arguments.arguments.get(name) else {
                return Ok(value.clone());
            };
            let line = source_line
                .get()
                .unwrap_or_else(|| argument.source_line(arguments.default_line));
            match argument.value_ref() {
                StructuralOperandRef::Value(value) => value.clone(),
                StructuralOperandRef::Handle { .. }
                    if matches!(position, ArgumentSubstitutionPosition::Value) =>
                {
                    return Err(ArgumentSubstitutionError {
                        source_file: None,
                        line,
                        message: BareStructuralHandle {
                            binding: diagnostic_name.as_deref().unwrap_or(name).to_string(),
                            kind: StructuralHandleKind::ModuleParameter,
                        }
                        .to_string(),
                        parse_error: None,
                    });
                }
                StructuralOperandRef::Handle { access_base, .. } => access_base.clone(),
                StructuralOperandRef::Parameter { access_base, .. } => {
                    let mut forwarded_value = access_base.clone();
                    if let Value::Deferred(DeferredValue::Unknown(
                        crate::resource::UnknownReason::ModuleArgument {
                            source_line: forwarded_line,
                            diagnostic_name: forwarded_diagnostic,
                            ..
                        },
                    )) = &mut forwarded_value
                    {
                        *forwarded_line = line.into();
                        *forwarded_diagnostic =
                            Some(diagnostic_name.as_deref().unwrap_or(name).to_string());
                    }
                    forwarded_value
                }
            }
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            let Some(argument) = arguments.arguments.get(binding) else {
                return Ok(value.clone());
            };
            let source_line = argument.source_line(arguments.default_line);
            argument
                .substitution(binding, ArgumentSubstitutionPosition::Value)
                .map_err(|message| ArgumentSubstitutionError {
                    source_file: None,
                    line: source_line,
                    message,
                    parse_error: None,
                })?
                .value()
                .clone()
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            let Some(argument) = arguments.arguments.get(path.binding()) else {
                return Ok(value.clone());
            };
            let source_line = argument.source_line(arguments.default_line);
            let substitution = argument
                .substitution(path.binding(), ArgumentSubstitutionPosition::AccessBase)
                .map_err(|message| ArgumentSubstitutionError {
                    source_file: None,
                    line: source_line,
                    message,
                    parse_error: None,
                })?;
            let mut substituted = apply_value_path_segment(
                substitution.value().clone(),
                PathSegment::Field {
                    name: path.attribute().to_string(),
                },
                source_line,
                AccessMissPolicy::Error,
            )
            .and_then(ValuePathSegmentResult::into_value)
            .map_err(|error| ArgumentSubstitutionError {
                source_file: None,
                line: source_line,
                message: error.to_string(),
                parse_error: None,
            })?;
            for segment in path.segments() {
                substituted = apply_value_path_segment(
                    substituted,
                    segment.clone(),
                    source_line,
                    AccessMissPolicy::Error,
                )
                .and_then(ValuePathSegmentResult::into_value)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: source_line,
                    message: error.to_string(),
                    parse_error: None,
                })?;
            }
            substituted
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            let value = substitute_arguments_with_position(
                value,
                arguments,
                ArgumentSubstitutionPosition::Value,
            )?;
            if matches!(value, Value::Concrete(_)) {
                value
            } else {
                Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(value),
                    default: Box::new(substitute_arguments_with_position(
                        default,
                        arguments,
                        ArgumentSubstitutionPosition::Value,
                    )?),
                })
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| {
                    substitute_arguments_with_position(
                        v,
                        arguments,
                        ArgumentSubstitutionPosition::Value,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut substituted = IndexMap::with_capacity(map.len());
            for (key, value) in map {
                substituted.insert(
                    key.clone(),
                    substitute_arguments_with_position(
                        value,
                        arguments,
                        ArgumentSubstitutionPosition::Value,
                    )?,
                );
            }
            Value::Concrete(ConcreteValue::Map(substituted))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let substituted_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => substitute_arguments_with_position(
                        v,
                        arguments,
                        ArgumentSubstitutionPosition::Value,
                    )
                    .map(InterpolationPart::Expr),
                    other => Ok(other.clone()),
                })
                .collect::<Result<_, _>>()?;
            Value::Deferred(DeferredValue::Interpolation(substituted_parts))
        }
        Value::Deferred(DeferredValue::FunctionCall { .. } | DeferredValue::Unavailable { .. }) => {
            let mut substituted = value.clone();
            let Value::Deferred(
                DeferredValue::FunctionCall { args, .. } | DeferredValue::Unavailable { args, .. },
            ) = &mut substituted
            else {
                unreachable!("matched a built-in call")
            };
            let substituted_args = args
                .iter()
                .map(|argument| substitute_function_argument(argument, arguments))
                .collect::<Result<_, _>>()?;
            *args = substituted_args;
            substituted
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let line = source_line.get().unwrap_or(function.body.source_line);
            let args = args
                .iter()
                .map(|argument| substitute_function_argument(argument, arguments))
                .collect::<Result<Vec<_>, _>>()?;
            crate::parser::evaluate_resolved_user_function(
                function,
                functions,
                &args,
                arguments.config,
                line,
            )
            .map_err(|error| function_substitution_error(error, line))?
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let substituted_base = substitute_arguments_with_position(
                access.base(),
                arguments,
                ArgumentSubstitutionPosition::AccessBase,
            )?;
            access
                .rebuild_with_base(substituted_base, crate::resource::AccessMissPolicy::Error)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: access.source_line(),
                    message: error.to_string(),
                    parse_error: None,
                })?
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => value.clone(),
    };

    if let (Some(binding_index), Value::Deferred(DeferredValue::ResourceRef { path })) =
        (arguments.binding_index, &substituted)
        && let RefType::UnknownAttribute(error @ RefTypeError::InvalidAccess { .. }) =
            binding_index.postfix_ref_type(path)
    {
        let line = match value {
            Value::Deferred(DeferredValue::Access(access)) => access.source_line(),
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::UnknownReason::ModuleArgument { source_line, .. },
            )) => source_line.get().unwrap_or(arguments.default_line),
            _ => arguments.default_line,
        };
        return Err(ArgumentSubstitutionError {
            source_file: None,
            line,
            message: error.to_string(),
            parse_error: None,
        });
    }

    Ok(substituted)
}

/// Rewrite intra-module ResourceRef binding names with instance path.
///
/// When a ResourceRef's binding_name matches one of the module's own bindings,
/// prefix it with dot notation so that each module instance has isolated references.
pub(super) fn rewrite_intra_module_refs(
    value: &Value,
    instance_prefix: &str,
    intra_module_bindings: &HashSet<String>,
) -> Value {
    match value {
        Value::Deferred(DeferredValue::BindingRef { binding })
            if intra_module_bindings.contains(binding) =>
        {
            Value::Deferred(DeferredValue::BindingRef {
                binding: apply_instance_prefix(instance_prefix, binding),
            })
        }
        Value::Deferred(DeferredValue::ResourceRef { path })
            if intra_module_bindings.contains(path.binding()) =>
        {
            Value::Deferred(DeferredValue::ResourceRef {
                path: crate::resource::AccessPath::with_segments(
                    apply_instance_prefix(instance_prefix, path.binding()),
                    path.attribute().to_string(),
                    path.segments().to_vec(),
                ),
            })
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    )
                })
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            let rewritten_parts: Vec<InterpolationPart> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => InterpolationPart::Expr(
                        rewrite_intra_module_refs(v, instance_prefix, intra_module_bindings),
                    ),
                    other => other.clone(),
                })
                .collect();
            Value::Deferred(DeferredValue::Interpolation(rewritten_parts))
        }
        Value::Deferred(DeferredValue::FunctionCall { .. } | DeferredValue::Unavailable { .. }) => {
            let mut rewritten = value.clone();
            let Value::Deferred(
                DeferredValue::FunctionCall { args, .. } | DeferredValue::Unavailable { args, .. },
            ) = &mut rewritten
            else {
                unreachable!("matched a built-in call")
            };
            let rewritten_args = args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(rewrite_intra_module_refs(
                                value,
                                instance_prefix,
                                intra_module_bindings,
                            ))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect();
            *args = rewritten_args;
            rewritten
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: functions.clone(),
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(rewrite_intra_module_refs(
                                value,
                                instance_prefix,
                                intra_module_bindings,
                            ))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect(),
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                rewrite_intra_module_refs(access.base(), instance_prefix, intra_module_bindings),
                crate::resource::AccessMissPolicy::Error,
            )
            .unwrap_or_else(|_| value.clone()),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(rewrite_intra_module_refs(
                    value,
                    instance_prefix,
                    intra_module_bindings,
                )),
                default: Box::new(rewrite_intra_module_refs(
                    default,
                    instance_prefix,
                    intra_module_bindings,
                )),
            })
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => value.clone(),
    }
}

/// Compute the instance prefix for a module call. Named calls use the
/// binding name; anonymous calls get `<module>_<16hex>` where the hex is a
/// SimHash of the call's module name + flattened arguments.
///
/// SimHash is locality-sensitive, so editing one argument flips only a few
/// bits — `reconcile_anonymous_module_instances` can then find the matching
/// state entry by Hamming distance and preserve the resource address across
/// argument edits.
pub fn instance_prefix_for_call(call: &ModuleCall) -> String {
    use std::collections::BTreeMap;

    if let Some(name) = &call.binding_name {
        return name.clone();
    }

    let mut features: BTreeMap<String, String> = BTreeMap::new();
    features.insert("_module".to_string(), call.module_name.clone());
    for (k, v) in call.arguments() {
        crate::identifier::flatten_value_for_simhash(k, v.walker_value(), &mut features);
    }
    let simhash = crate::identifier::compute_simhash(&features);
    format!("{}_{:016x}", call.module_name, simhash)
}

/// Split a module-instance prefix into `(module_name, simhash)` when the
/// tail looks like a 16-hex SimHash. Returns `None` for non-synthetic prefixes
/// (user-written binding names, pre-SimHash state formats, etc.).
pub(super) fn parse_synthetic_instance_prefix(prefix: &str) -> Option<(&str, SimHash)> {
    let (module, hex) = prefix.rsplit_once('_')?;
    let simhash = SimHash::parse_16_hex(hex)?;
    if module.is_empty() {
        return None;
    }
    Some((module, simhash))
}

/// Split a resource name into `(instance_prefix, rest)` at the first `.`, or
/// return `None` if it has no dot (no module instance prefix at all).
pub(super) fn split_instance_prefix(name: &str) -> Option<(&str, &str)> {
    name.split_once('.')
}

/// Reconcile anonymous module-instance prefixes with existing state.
///
/// When a user edits an argument of an anonymous module call, its SimHash
/// prefix shifts a few bits. The expanded resources therefore live under a
/// new address (e.g. `thing_ab12….role` → `thing_cd34….role`) and would
/// otherwise look like destroy + create to the differ. This pass detects the
/// case by Hamming-distance matching: for each current DSL instance prefix
/// whose address is absent from state, find a state-only prefix for the same
/// module within `SIMHASH_HAMMING_THRESHOLD` bits; if exactly one candidate
/// qualifies, rewrite the current resources and their retained compositions
/// to use the state address.
///
/// `find_state_names_by_type` returns every state resource name for a given
/// `(provider, resource_type)` — the reconciler uses them to discover which
/// instance prefixes already exist in state.
pub fn reconcile_anonymous_module_instances(
    resources: &mut [Resource],
    compositions: &mut [LiveComposition],
    find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
    claims: &crate::identifier::StateBlockClaims,
) {
    use std::collections::{HashMap, HashSet};

    // Collect current (provider, resource_type) pairs that appear in the
    // expanded DSL — we'll query state for matching entries.
    let mut touched_types: HashSet<(String, String)> = HashSet::new();
    for r in resources.iter() {
        let Some(identity) = r.id.identity_str() else {
            continue;
        };
        if split_instance_prefix(identity).is_none() {
            continue;
        }
        touched_types.insert((r.id.provider.clone(), r.id.resource_type.clone()));
    }

    if touched_types.is_empty() {
        return;
    }

    // Current DSL synthetic prefixes per module — only one entry per
    // distinct prefix (a multi-resource module instance shares one prefix
    // across all of its resources).
    let mut current_synthetic_by_module: HashMap<String, HashSet<SimHash>> = HashMap::new();
    let mut claimed_current_by_module: HashMap<String, HashSet<SimHash>> = HashMap::new();
    for r in resources.iter() {
        let Some(identity) = r.id.identity_str() else {
            continue;
        };
        let Some((prefix, _)) = split_instance_prefix(identity) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        current_synthetic_by_module
            .entry(module.to_string())
            .or_default()
            .insert(simhash);
        if claims.claims_to(&r.id) {
            claimed_current_by_module
                .entry(module.to_string())
                .or_default()
                .insert(simhash);
        }
    }

    // State synthetic prefixes per module. Use a set so a multi-resource
    // module instance — which contributes one state entry per resource
    // type, all under the same prefix — collapses to one candidate. With a
    // Vec the same hash would appear N times and the Hamming-distance
    // search below would mistake duplicates for ambiguous candidates and
    // refuse to remap (#2211).
    let mut state_synthetic_by_module: HashMap<String, HashSet<SimHash>> = HashMap::new();
    let mut claimed_state_by_module: HashMap<String, HashSet<SimHash>> = HashMap::new();

    for (provider, resource_type) in &touched_types {
        let state_names = claims.screen_entries(
            provider,
            resource_type,
            find_state_names_by_type(provider, resource_type),
            |name| name,
        );
        for name in state_names.all() {
            let Some((prefix, _)) = split_instance_prefix(name) else {
                continue;
            };
            let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
                continue;
            };
            state_synthetic_by_module
                .entry(module.to_string())
                .or_default()
                .insert(simhash);
        }
        for name in state_names.claimed() {
            let Some((prefix, _)) = split_instance_prefix(name) else {
                continue;
            };
            let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
                continue;
            };
            claimed_state_by_module
                .entry(module.to_string())
                .or_default()
                .insert(simhash);
        }
    }

    // For each current DSL prefix that has no matching state prefix, find the
    // closest orphan state prefix for the same module. Candidate state hashes
    // exclude any prefix already used by a current DSL instance — without
    // that filter, two distinct anonymous calls could collapse onto the same
    // state entry when only one of them existed before.
    let mut prefix_remap: HashMap<(String, SimHash), SimHash> = HashMap::new();
    for (module, current_hashes) in &current_synthetic_by_module {
        let Some(state_hashes) = state_synthetic_by_module.get(module) else {
            continue;
        };
        let orphan_state_hashes: Vec<SimHash> = state_hashes
            .iter()
            .copied()
            .filter(|h| !current_hashes.contains(h))
            .filter(|h| {
                !claimed_state_by_module
                    .get(module)
                    .is_some_and(|hashes| hashes.contains(h))
            })
            .collect();
        if orphan_state_hashes.is_empty() {
            continue;
        }
        for current_hash in current_hashes {
            if state_hashes.contains(current_hash) {
                continue;
            }
            if claimed_current_by_module
                .get(module)
                .is_some_and(|hashes| hashes.contains(current_hash))
            {
                continue;
            }
            if let Some(state_hash) = crate::identifier::closest_unique_simhash_match(
                *current_hash,
                orphan_state_hashes.iter().copied(),
                |h| h,
            ) {
                prefix_remap.insert((module.clone(), *current_hash), state_hash);
            }
        }
    }

    if prefix_remap.is_empty() {
        return;
    }

    // Apply remaps: rewrite identity and `binding` for every resource whose
    // instance prefix is in the remap table.
    for r in resources.iter_mut() {
        let Some(identity) = r.id.identity_str() else {
            continue;
        };
        let Some((prefix, rest)) = split_instance_prefix(identity) else {
            continue;
        };
        let Some((module, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        if let Some(&target) = prefix_remap.get(&(module.to_string(), simhash)) {
            let old_prefix = prefix.to_string();
            let new_prefix = format!("{}_{:016x}", module, target);
            let new_name = format!("{}.{}", new_prefix, rest);
            r.id.set_identity(ResourceIdentity::new(new_name));
            r.loop_provenance.remap_identity_prefix(
                &ResourceIdentity::new(old_prefix.clone()),
                &ResourceIdentity::new(new_prefix.clone()),
            );
            if let Some(ref binding) = r.binding
                && let Some((_, binding_rest)) = split_instance_prefix(binding)
            {
                r.binding = Some(format!("{}.{}", new_prefix, binding_rest));
            }
            if let Some(crate::resource::ModuleSource::Module(module)) = &r.module_source {
                let mut remapped_call_path = module.module_call_path().clone();
                for call in remapped_call_path.calls_mut() {
                    if let Some(rewritten) = rewrite_name_prefix(&call.instance, &prefix_remap) {
                        call.instance = rewritten;
                    }
                    if let Some(binding) = call.binding.as_deref()
                        && let Some(rewritten) = rewrite_name_prefix(binding, &prefix_remap)
                    {
                        call.binding = Some(rewritten);
                    }
                }
                r.module_source = Some(crate::resource::ModuleSource::expanded_module(
                    module.name(),
                    new_prefix.clone(),
                    module
                        .identity_scope()
                        .remap_prefix(&old_prefix, &new_prefix),
                    remapped_call_path,
                ));
            }
        }
    }

    // After remapping resource names, every expanded surface that names the
    // anonymous instance must move with it. That includes managed-resource
    // references plus composition identities, bindings, arguments, and
    // outputs retained for post-expansion validation and post-apply export
    // resolution.
    for r in resources.iter_mut() {
        let mut replacements = Vec::new();
        for (key, value) in r.attributes.iter() {
            let rewritten = rewrite_ref_prefixes(value, &prefix_remap);
            if rewritten != *value {
                replacements.push((key.clone(), rewritten));
            }
        }
        for (key, new_value) in replacements {
            r.set_attr(key, new_value);
        }
    }

    for composition in compositions {
        if let Some(identity) = rewrite_name_prefix(composition.id.identity_str(), &prefix_remap) {
            composition.id = composition
                .id
                .instantiate_with_identity(ResourceIdentity::new(identity));
        }
        if let Some(binding) = composition.binding.as_deref()
            && let Some(rewritten) = rewrite_name_prefix(binding, &prefix_remap)
        {
            composition.binding = Some(rewritten);
        }
        if let Some(rewritten) = rewrite_name_prefix(&composition.instance, &prefix_remap) {
            composition.instance = rewritten;
        }
        for call in composition.call_path.calls_mut() {
            if let Some(rewritten) = rewrite_name_prefix(&call.instance, &prefix_remap) {
                call.instance = rewritten;
            }
            if let Some(binding) = call.binding.as_deref()
                && let Some(rewritten) = rewrite_name_prefix(binding, &prefix_remap)
            {
                call.binding = Some(rewritten);
            }
        }

        for argument in composition.signature.arguments.values_mut() {
            let rewritten = rewrite_ref_prefixes(argument.value(), &prefix_remap);
            *argument = argument.with_value(rewritten);
        }
        for attribute in composition.signature.attributes.values_mut() {
            let rewritten = rewrite_ref_prefixes(&attribute.to_value(), &prefix_remap);
            *attribute = attribute.with_value(rewritten);
        }
    }
}

fn rewrite_name_prefix(
    name: &str,
    remap: &std::collections::HashMap<(String, SimHash), SimHash>,
) -> Option<String> {
    let (prefix, suffix) = name
        .split_once('.')
        .map_or((name, None), |(prefix, rest)| (prefix, Some(rest)));
    let (module, simhash) = parse_synthetic_instance_prefix(prefix)?;
    let target = remap.get(&(module.to_string(), simhash))?;
    let new_prefix = format!("{}_{:016x}", module, target);
    Some(match suffix {
        Some(rest) => format!("{new_prefix}.{rest}"),
        None => new_prefix,
    })
}

fn rewrite_ref_prefixes(
    value: &Value,
    remap: &std::collections::HashMap<(String, SimHash), SimHash>,
) -> Value {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            if let Some(binding) = rewrite_name_prefix(path.binding(), remap) {
                return Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::with_segments(
                        binding,
                        path.attribute().to_string(),
                        path.segments().to_vec(),
                    ),
                });
            }
            value.clone()
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            rewrite_name_prefix(binding, remap).map_or_else(
                || value.clone(),
                |binding| Value::Deferred(DeferredValue::BindingRef { binding }),
            )
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|v| rewrite_ref_prefixes(v, remap))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), rewrite_ref_prefixes(v, remap)))
                .collect(),
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            use crate::resource::InterpolationPart;
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|p| match p {
                        InterpolationPart::Literal(s) => InterpolationPart::Literal(s.clone()),
                        InterpolationPart::Expr(v) => {
                            InterpolationPart::Expr(rewrite_ref_prefixes(v, remap))
                        }
                    })
                    .collect(),
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name: *name,
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(rewrite_ref_prefixes(value, remap))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect(),
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: functions.clone(),
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| {
                            Ok::<_, std::convert::Infallible>(rewrite_ref_prefixes(value, remap))
                        })
                        .expect("infallible function-argument rewrite")
                })
                .collect(),
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                rewrite_ref_prefixes(access.base(), remap),
                crate::resource::AccessMissPolicy::Error,
            )
            .unwrap_or_else(|_| value.clone()),
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(rewrite_ref_prefixes(inner, remap)),
        )),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(rewrite_ref_prefixes(value, remap)),
                default: Box::new(rewrite_ref_prefixes(default, remap)),
            })
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_expanded_call() -> CompositionCall {
        let parsed = crate::parser::parse_file(
            "let registry = use { source = './registry' }\nlet registry_publish = registry {}\n",
            std::path::Path::new("data-source-call.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("parse module owner call");
        let call = &parsed.module_calls[0];
        CompositionCall {
            id: call.id().clone(),
            module_name: call.module_name.clone(),
            binding: call.binding_name.clone(),
            instance: "registry_publish".to_string(),
            module_source: Some("./registry".to_string()),
            module_directory: None,
            loop_origin: None,
        }
    }

    #[test]
    fn prefix_module_data_source_prefixes_non_empty_identity_and_binding() {
        let mut data_source =
            DataSource::with_provider("aws", "sts.CallerIdentity", "caller", None)
                .with_binding("caller");
        data_source
            .dependency_bindings
            .extend(["target".to_string(), "module_argument".to_string()]);

        let arguments = HashMap::new();
        let config = crate::parser::ProviderContext::default();
        let substitutions = ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        };
        let prefixed = prefix_module_data_source(
            &data_source,
            "registry_publish",
            "registry",
            &ModuleInstanceScope::for_call(
                "registry",
                Some("registry_publish"),
                "registry_publish",
            ),
            &test_expanded_call(),
            &HashSet::from(["caller".to_string(), "target".to_string()]),
            substitutions,
        )
        .expect("prefix data source");

        assert_eq!(
            prefixed.id.identity_str().expect("resolved identity"),
            "registry_publish.caller"
        );
        assert_eq!(prefixed.binding.as_deref(), Some("registry_publish.caller"));
        assert_eq!(
            prefixed.dependency_bindings,
            BTreeSet::from([
                "module_argument".to_string(),
                "registry_publish.target".to_string(),
            ])
        );
    }

    #[test]
    fn prefix_module_resource_prefixes_only_intra_module_dependency_bindings() {
        let mut resource =
            Resource::with_provider("aws", "iam.Role", "consumer", None).with_binding("consumer");
        resource.dependency_bindings.extend([
            "target".to_string(),
            "module_argument".to_string(),
            "outer_binding".to_string(),
        ]);

        let arguments = HashMap::new();
        let config = crate::parser::ProviderContext::default();
        let substitutions = ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        };
        let prefixed = prefix_module_resource(
            &resource,
            "registry_publish",
            "registry",
            &ModuleInstanceScope::for_call(
                "registry",
                Some("registry_publish"),
                "registry_publish",
            ),
            &test_expanded_call(),
            &HashSet::from(["consumer".to_string(), "target".to_string()]),
            substitutions,
        )
        .expect("prefix resource");

        assert_eq!(
            prefixed.dependency_bindings,
            BTreeSet::from([
                "module_argument".to_string(),
                "outer_binding".to_string(),
                "registry_publish.target".to_string(),
            ])
        );
    }

    #[test]
    fn prefix_module_resource_prefixes_loop_legacy_and_current_paths() {
        let parsed = crate::parser::parse_file(
            "for value in ['one'] {\n  mock.test.Leaf { value = value }\n}\n",
            std::path::Path::new("module/loop.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("loop fixture parses");
        let resource = &parsed.resources[0];
        let call_origin = resource
            .loop_provenance
            .direct_origin()
            .expect("direct loop origin")
            .clone();
        let mut module_call = test_expanded_call();
        module_call.set_loop_origin(Some(call_origin));

        let arguments = HashMap::new();
        let config = crate::parser::ProviderContext::default();
        let substitutions = ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        };
        let prefixed = prefix_module_resource(
            resource,
            "registry_publish",
            "registry",
            &ModuleInstanceScope::for_call(
                "registry",
                Some("registry_publish"),
                "registry_publish",
            ),
            &module_call,
            &HashSet::new(),
            substitutions,
        )
        .expect("prefix loop resource");

        assert_eq!(
            prefixed
                .loop_provenance
                .direct_origin()
                .expect("direct origin retained")
                .legacy_address()
                .exact_state_name(),
            "_for0[0]._for0[0]",
            "an inner loop resource must compose the outer module-loop legacy address",
        );
        let [module_prefix] = prefixed.loop_provenance.module_prefixes() else {
            panic!("loop-generated module call must add one prefix origin");
        };
        assert_eq!(module_prefix.current_prefix().as_str(), "registry_publish");
        assert_eq!(module_prefix.legacy_prefix().exact_state_name(), "_for0[0]");
    }

    #[test]
    fn anonymous_module_reconcile_remaps_loop_current_prefix_only() {
        let seed = crate::parser::parse_file(
            "for value in ['one'] {\n  mock.test.Leaf { value = value }\n}\n",
            std::path::Path::new("root.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("loop fixture parses")
        .resources
        .remove(0);
        let origin = seed
            .loop_provenance
            .direct_origin()
            .expect("loop origin")
            .clone();
        let current_hash =
            crate::identifier::compute_simhash(&std::collections::BTreeMap::from([(
                "argument",
                "current".to_string(),
            )]));
        let state_hash = current_hash.with_flipped_mask_for_test(1);
        let current_prefix = format!("web_{current_hash:016x}");
        let state_prefix = format!("web_{state_hash:016x}");
        let mut resource = Resource::with_provider(
            "mock",
            "test.Leaf",
            ResourceIdentity::new(format!("{current_prefix}.leaf")),
            None,
        );
        let mut provenance = crate::resource::LoopProvenance::default();
        provenance.push_module_prefix(crate::resource::LoopModulePrefixOrigin::new(
            origin,
            ResourceIdentity::new(current_prefix.clone()),
        ));
        resource.loop_provenance = provenance;
        let mut resources = vec![resource];

        reconcile_anonymous_module_instances(
            &mut resources,
            &mut [],
            &|_, _| vec![format!("{state_prefix}.leaf")],
            &crate::identifier::StateBlockClaims::empty(),
        );

        let expected_name = format!("{state_prefix}.leaf");
        assert_eq!(resources[0].id.identity_str(), Some(expected_name.as_str()));
        let [module_prefix] = resources[0].loop_provenance.module_prefixes() else {
            panic!("module prefix provenance retained");
        };
        assert_eq!(module_prefix.current_prefix().as_str(), state_prefix);
        assert_eq!(
            module_prefix.legacy_prefix().exact_state_name(),
            "_for0[0]",
            "legacy migration oracle must not follow current SimHash reconciliation",
        );
    }

    #[test]
    fn anonymous_module_reconcile_remaps_current_prefixed_direct_legacy_address() {
        let seed = crate::parser::parse_file(
            "for value in ['one'] {\n  mock.test.Leaf { value = value }\n}\n",
            std::path::Path::new("module/loop.crn"),
            &crate::parser::ProviderContext::default(),
        )
        .expect("loop fixture parses")
        .resources
        .remove(0);
        let mut direct = seed
            .loop_provenance
            .direct_origin()
            .expect("loop origin")
            .clone();
        let current_hash =
            crate::identifier::compute_simhash(&std::collections::BTreeMap::from([(
                "argument",
                "current".to_string(),
            )]));
        let state_hash = current_hash.with_flipped_mask_for_test(1);
        let current_prefix = format!("web_{current_hash:016x}");
        let state_prefix = format!("web_{state_hash:016x}");
        direct.prefix_legacy(&ResourceIdentity::new(current_prefix.clone()));

        let mut resource = Resource::with_provider(
            "mock",
            "test.Leaf",
            ResourceIdentity::new(format!("{current_prefix}.leaf")),
            None,
        );
        resource.loop_provenance = crate::resource::LoopProvenance::direct(direct);
        let mut resources = vec![resource];

        reconcile_anonymous_module_instances(
            &mut resources,
            &mut [],
            &|_, _| vec![format!("{state_prefix}.leaf")],
            &crate::identifier::StateBlockClaims::empty(),
        );

        let expected_name = format!("{state_prefix}.leaf");
        assert_eq!(resources[0].id.identity_str(), Some(expected_name.as_str()));
        assert_eq!(
            resources[0]
                .loop_provenance
                .direct_origin()
                .expect("direct origin retained")
                .legacy_address()
                .exact_state_name(),
            format!("{state_prefix}._for0[0]"),
            "legacy migration candidates derived from an anonymous module prefix must move with reconciliation",
        );
    }

    #[test]
    fn prefix_module_data_source_prefixes_managed_dependency_binding() {
        let mut roles =
            DataSource::with_provider("mock", "iam.Roles", "roles", None).with_binding("roles");
        roles.dependency_bindings.insert("target".to_string());

        let expanded_call = test_expanded_call();

        let intra_module_bindings = HashSet::from(["target".to_string(), "roles".to_string()]);
        let arguments = HashMap::new();
        let config = crate::parser::ProviderContext::default();
        let substitutions = ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        };
        let roles = prefix_module_data_source(
            &roles,
            "registry_publish",
            "registry",
            &ModuleInstanceScope::for_call(
                "registry",
                Some("registry_publish"),
                "registry_publish",
            ),
            &expanded_call,
            &intra_module_bindings,
            substitutions,
        )
        .expect("prefix data source");

        assert_eq!(
            roles.dependency_bindings,
            BTreeSet::from(["registry_publish.target".to_string()]),
        );
        assert_eq!(
            roles
                .module_source
                .as_ref()
                .and_then(ModuleSource::module_call)
                .map(|source| source.module_call_path()),
            Some(&CompositionCallPath::direct(expanded_call)),
        );
    }
}
