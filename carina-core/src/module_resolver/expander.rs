//! Module-call expansion, argument substitution, and intra-module
//! reference rewriting.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use crate::binding_index::{BindingIndex, RefType, RefTypeError};
use crate::eval_value::{BareStructuralHandle, StructuralHandleKind};
use crate::identifier::SimHash;
use crate::parser::{
    ArgumentOrigin, BareBindingSource, DeferredForBody, DeferredIfBody, DeferredIfCondition,
    DeferredStructuralIf, DeferredValueIf, ModuleArgument, ModuleCall, ParseError,
    ParsedBindingUses, ParsedDeferredForExpression, ParsedDeferredForIterableState, ParsedFile,
    RelativeBindingName, WaitBinding,
};
use crate::resource::{
    AbsoluteBindingAddress, AccessMissPolicy, ApplyTimeUnknown, AuthoredDataSource as DataSource,
    AuthoredDeferredValue as DeferredValue, AuthoredResource as Resource, AuthoredValue as Value,
    BindingAddress, ConcreteValue, LegacyLoopAddress, LoopSlot, ModuleInstanceScope, ModuleSource,
    ParserPlaceholder, PathSegment, PlaceholderKind, ResourceId, ResourceIdentity,
    ResourceIdentityState, StructuralOperandRef, ValuePathSegmentResult, apply_value_path_segment,
};

use super::error::ModuleError;
use super::resolver::{
    ModuleCallConstraintReport, ModuleResolver, ResolvedModuleConstraintOrigin,
    ResolvedModuleConstraintOutcome, ResolvedModuleConstraintStatus, TypeResolutionPolicy,
};
use super::validation::{
    ConstraintEvaluation, ModuleConstraintKind, ModuleConstraints, evaluate_module_constraints,
};

type ModuleResolvedValue = crate::resource::Value<ParserPlaceholder, AbsoluteBindingAddress>;
type ModuleResolvedResource =
    crate::resource::Resource<ModuleSource, ParserPlaceholder, AbsoluteBindingAddress>;
type ModuleResolvedDataSource =
    crate::resource::DataSource<ModuleSource, ParserPlaceholder, AbsoluteBindingAddress>;
type ModuleResolvedLiveComposition =
    crate::resource::LiveComposition<ParserPlaceholder, AbsoluteBindingAddress>;

fn intra_module_binding_names(module: &ParsedFile) -> HashSet<RelativeBindingName> {
    module
        .resources
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .chain(
            module
                .data_sources
                .iter()
                .filter_map(|data_source| data_source.binding.clone()),
        )
        .chain(module.wait_bindings.iter().map(|wait| wait.binding.clone()))
        .chain(
            module
                .module_calls
                .iter()
                .filter_map(|call| call.binding_name.clone()),
        )
        // Nested compositions may already carry a multi-segment binding
        // (`middle.inner`). The outer expansion owns the final scope lift.
        .chain(
            module
                .compositions
                .iter()
                .filter_map(|composition| composition.binding.clone()),
        )
        .chain(
            module
                .parsed_deferred_for_expressions()
                .iter()
                .filter_map(|deferred| match &deferred.target {
                    crate::parser::DeferredForTarget::Bound { binding } => Some(binding.clone()),
                    crate::parser::DeferredForTarget::Unbound { template: _ } => None,
                }),
        )
        .chain(
            module
                .parsed_deferred_for_expressions()
                .iter()
                .filter_map(|deferred| match &deferred.body {
                    DeferredForBody::Resource(resource) => resource.binding.clone(),
                    DeferredForBody::Read(data_source) => data_source.binding.clone(),
                    DeferredForBody::ModuleCall(call) => call.binding_name.clone(),
                }),
        )
        .chain(
            module
                .deferred_module_ifs
                .iter()
                .flat_map(DeferredStructuralIf::branches)
                .filter_map(DeferredIfBody::binding_address)
                .cloned(),
        )
        .collect()
}
struct ModuleExpansionSource<'a> {
    module: Option<&'a ParsedFile>,
    source: Option<&'a str>,
    directory: Option<&'a std::path::Path>,
    scope: Option<&'a ModuleInstanceScope>,
    parent_call_path: Option<&'a crate::resource::ExpandedCompositionCallPath>,
}

impl ModuleExpansionSource<'_> {
    fn imported() -> Self {
        Self {
            module: None,
            source: None,
            directory: None,
            scope: None,
            parent_call_path: None,
        }
    }
}

impl<P: TypeResolutionPolicy> ModuleResolver<'_, P> {
    /// Expand a module call into resources.
    ///
    /// Every module call creates a [`crate::resource::LiveComposition`]. Bound calls expose module
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
        authored_call: &ModuleCall,
        call: &crate::parser::ModuleCall<ParserPlaceholder, AbsoluteBindingAddress>,
        instance_prefix: &str,
        ref_check: &super::resolver::RefCheckWitness,
        binding_index: Option<&BindingIndex<'_>>,
        resolved_bindings: Option<&crate::binding_index::ResolvedBindings<ParserPlaceholder>>,
    ) -> Result<crate::parser::RootBindingResolvedParsedFile, ModuleError> {
        match ref_check {
            super::resolver::RefCheckWitness(()) => {}
        }
        let expanded = self
            .expand_module_call_inner(
                authored_call,
                call,
                instance_prefix,
                binding_index,
                resolved_bindings,
                ModuleExpansionSource::imported(),
            )
            .map_err(|error| error.with_failing_call(call.id().clone()))?;
        if let Some(error) =
            ModuleError::from_errors(super::resolver::deferred_for_body_errors(&expanded))
        {
            return Err(error.with_failing_call(call.id().clone()));
        }
        Ok(expanded)
    }

    pub(super) fn expand_module_call_checked_collecting(
        &self,
        authored_call: &ModuleCall,
        call: &crate::parser::ModuleCall<ParserPlaceholder, AbsoluteBindingAddress>,
        instance_prefix: &str,
        ref_check: &super::resolver::RefCheckWitness,
        binding_index: Option<&BindingIndex<'_>>,
        resolved_bindings: Option<&crate::binding_index::ResolvedBindings<ParserPlaceholder>>,
    ) -> Result<crate::parser::RootBindingResolvedParsedFile, Vec<ModuleError>> {
        match ref_check {
            super::resolver::RefCheckWitness(()) => {}
        }
        let expanded = self
            .expand_module_call_inner(
                authored_call,
                call,
                instance_prefix,
                binding_index,
                resolved_bindings,
                ModuleExpansionSource::imported(),
            )
            .map_err(|error| vec![error.with_failing_call(call.id().clone())])?;
        let errors = super::resolver::deferred_for_body_errors(&expanded)
            .into_iter()
            .map(|error| error.with_failing_call(call.id().clone()))
            .collect::<Vec<_>>();
        if errors.is_empty() {
            Ok(expanded)
        } else {
            Err(errors)
        }
    }

    fn expand_module_call_inner(
        &self,
        authored_call: &ModuleCall,
        call: &crate::parser::ModuleCall<ParserPlaceholder, AbsoluteBindingAddress>,
        instance_prefix: &str,
        binding_index: Option<&BindingIndex<'_>>,
        resolved_bindings: Option<&crate::binding_index::ResolvedBindings<ParserPlaceholder>>,
        expansion_source: ModuleExpansionSource<'_>,
    ) -> Result<crate::parser::RootBindingResolvedParsedFile, ModuleError> {
        let module = match expansion_source.module {
            Some(module) => module,
            None => self
                .imported_modules
                .get(&call.module_name)
                .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?,
        };
        let call_source_file = call.source_file();
        let module_instance_scope = expansion_source.scope.cloned().unwrap_or_else(|| {
            ModuleInstanceScope::for_call(
                &call.module_name,
                call.binding_name
                    .as_ref()
                    .map(AbsoluteBindingAddress::as_str),
                instance_prefix,
            )
        });
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
        let authored_argument_references = authored_call
            .arguments()
            .filter_map(|(name, argument)| {
                authored_argument_reference_path(argument)
                    .map(|reference| (name.to_string(), reference))
            })
            .collect::<HashMap<_, _>>();
        let mut resolved_arguments: HashMap<String, ResolvedArgument> = HashMap::new();
        let call_line = call.source_span().start_line();
        for arg in &module.arguments {
            let forwarded_from_enclosing_parameter = authored_call
                .authored_argument(&arg.name)
                .is_some_and(|authored| {
                    crate::parser::contains_module_argument_matching(
                        authored.argument().walker_value(),
                        &mut |_| true,
                    )
                });
            let argument = match call.authored_argument(&arg.name) {
                Some(authored) => ResolvedArgument {
                    argument: authored.argument().clone(),
                    origin: ArgumentOrigin::Authored {
                        key_span: authored.key_span(),
                        value_span: authored.value_span(),
                    },
                    caller_reference: authored_argument_references.get(&arg.name).cloned(),
                    forwarded_from_enclosing_parameter,
                },
                None => ResolvedArgument {
                    argument: ModuleArgument::new(
                        arg.default
                            .clone()
                            .expect("required arguments were checked above")
                            .resolve_module(&module_instance_scope),
                    ),
                    origin: ArgumentOrigin::Defaulted,
                    caller_reference: None,
                    forwarded_from_enclosing_parameter: false,
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
                config: self.config(),
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
                let caller_reference = current.caller_reference.clone().or_else(|| {
                    module_parameter_postfix(current.walker_value(), &snapshot).and_then(
                        |(source, postfix)| {
                            snapshot.get(&source).and_then(|source_argument| {
                                source_argument
                                    .caller_reference
                                    .clone()
                                    .map(|reference| append_reference_path(reference, &postfix))
                            })
                        },
                    )
                });
                let forwarded_from_enclosing_parameter = current.forwarded_from_enclosing_parameter
                    || crate::parser::contains_module_argument_matching(
                        current.walker_value(),
                        &mut |name| {
                            snapshot
                                .get(name)
                                .is_some_and(|argument| argument.forwarded_from_enclosing_parameter)
                        },
                    );
                if next != current.argument
                    || caller_reference != current.caller_reference
                    || forwarded_from_enclosing_parameter
                        != current.forwarded_from_enclosing_parameter
                {
                    resolved_arguments.insert(
                        arg.name.clone(),
                        ResolvedArgument {
                            argument: next,
                            origin: current.origin,
                            caller_reference,
                            forwarded_from_enclosing_parameter,
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
            config: self.config(),
        };
        let argument_values: HashMap<String, ModuleResolvedValue> = resolved_arguments
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
                ArgumentOrigin::Authored {
                    key_span: _,
                    value_span,
                } => value_span.start_line(),
                ArgumentOrigin::Defaulted => call_line,
            };
            let static_type = P::resolve_static_type(&arg.static_type, self.config());
            let Some(type_expr) = static_type.known() else {
                continue;
            };
            let Some(resolved_parameter) =
                crate::validation::resolve_parameter_type_for_check(type_expr, self.config())
                    .map_err(|error| {
                        ModuleError::Parse(ParseError::InvalidExpression {
                            line,
                            message: error.to_string(),
                        })
                    })?
            else {
                continue;
            };
            match argument.value_ref() {
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } => {
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
                StructuralOperandRef::Parameter {
                    name: _,
                    access_base: _,
                } => continue,
                StructuralOperandRef::Value(_) => {}
            }
            let value = argument
                .value()
                .expect("value-shaped module argument was matched above");
            let resolved_type = resolved_parameter.as_type_expr();
            self.provider_context
                .check(
                    &resolved_type,
                    value,
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
                                            call.binding_name
                                                .as_ref()
                                                .map(AbsoluteBindingAddress::as_str),
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
            let report =
                ModuleCallConstraintReport::new(source, instance_prefix.to_string(), outcomes);
            self.record_constraint_report(report);
        }

        let mut pending_constraints = Vec::new();
        for evaluated in evaluated_constraints {
            match evaluated.evaluation() {
                ConstraintEvaluation::Satisfied => {}
                ConstraintEvaluation::Pending => {
                    pending_constraints.push(evaluated.into_constraint());
                }
                ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_)
                    if self.records_constraint_reports() => {}
                ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_) => {
                    return Err(evaluated
                        .module_error(
                            &call.module_name,
                            instance_prefix,
                            call.binding_name
                                .as_ref()
                                .map(AbsoluteBindingAddress::as_str),
                            Some(call.id()),
                        )
                        .expect("concrete constraint failure has a resolver error"));
                }
            }
        }

        // Collect every binding declared by this module. The shared scope
        // resolver accepts exactly these locals, substitutes declared
        // arguments with caller-expanded values, and rejects every other
        // relative name.
        let intra_module_bindings = intra_module_binding_names(module);
        let module_source = expansion_source
            .source
            .map(str::to_string)
            .or_else(|| self.module_paths.get(&call.module_name).cloned());
        let module_directory = expansion_source
            .directory
            .map(std::path::Path::to_path_buf)
            .or_else(|| {
                module_source.as_deref().map(|source| {
                    let source = std::path::Path::new(source);
                    let path = if source.is_absolute() {
                        source.to_path_buf()
                    } else {
                        self.base_dir.join(source)
                    };
                    path.canonicalize().unwrap_or(path)
                })
            });
        let expanded_call = crate::resource::ExpandedCompositionCall {
            id: call.id().clone(),
            module_name: call.module_name.clone(),
            binding: call.binding_name.clone(),
            instance: instance_prefix.to_string(),
            module_source: module_source.clone(),
            module_directory: module_directory.clone(),
            loop_origin: call.loop_origin().cloned().map(Box::new),
        };
        let expanded_call_path = match expansion_source.parent_call_path {
            Some(parent) => parent.clone().append(expanded_call.clone()),
            None => crate::resource::CompositionCallPath::direct(expanded_call.clone()),
        };
        let module_scope_resolver = ModuleScopeResolver {
            scope: &module_instance_scope,
            local_bindings: &intra_module_bindings,
            arguments: argument_substitutions,
            call_path: &expanded_call_path,
            argument_placeholders: argument_substitutions
                .arguments
                .keys()
                .map(|name| {
                    let placeholder = Value::Deferred(DeferredValue::Placeholder(
                        ParserPlaceholder::ModuleArgument {
                            name: name.clone(),
                            diagnostic: Box::default(),
                        },
                    ));
                    (
                        name.clone(),
                        ResolvedArgument::<RelativeBindingName> {
                            argument: ModuleArgument::new(placeholder),
                            origin: ArgumentOrigin::Defaulted,
                            caller_reference: None,
                            forwarded_from_enclosing_parameter: false,
                        },
                    )
                })
                .collect(),
        };

        // A module-argument condition is decided only after the call-site
        // values have crossed the typed module scope. Selected bodies remain
        // authored/relative until the normal expansion routines below resolve
        // them through `ModuleInstanceScope`.
        let selected_module_ifs =
            select_deferred_module_ifs(module, &expanded_call, &module_scope_resolver)
                .map_err(contextualize_access_error)?;
        let selected = &selected_module_ifs.contribution;

        let nested_binding_index = BindingIndex::from_parsed(module, self.schemas);
        let mut nested_contributions = Vec::new();
        for nested_call in module.module_calls.iter().chain(&selected.module_calls) {
            let local_prefix = instance_prefix_for_call(nested_call);
            let local_scope = ModuleInstanceScope::for_call(
                &nested_call.module_name,
                nested_call
                    .binding_name
                    .as_ref()
                    .map(RelativeBindingName::as_str),
                &local_prefix,
            );
            let child_scope = module_instance_scope.with_child(&local_scope);
            let expanded_nested_call =
                resolve_nested_module_call(nested_call, &module_scope_resolver)
                    .map_err(contextualize_access_error)?;
            let (child_module, child_source, child_directory) =
                self.nested_module_definition(module, module_directory.as_deref(), nested_call)?;
            let expanded = self
                .expand_module_call_inner(
                    nested_call,
                    &expanded_nested_call,
                    child_scope.key(),
                    Some(&nested_binding_index),
                    resolved_bindings,
                    ModuleExpansionSource {
                        module: Some(child_module),
                        source: Some(&child_source),
                        directory: Some(&child_directory),
                        scope: Some(&child_scope),
                        parent_call_path: Some(&expanded_call_path),
                    },
                )
                .map_err(|error| error.with_failing_call(nested_call.id().clone()))?;
            nested_contributions.push(expanded);
        }

        // Expand managed resources with substituted values.
        let mut resources: Vec<ModuleResolvedResource> = Vec::new();
        let mut expanded_parser_metadata: crate::parser::ParserBindingMetadata<
            AbsoluteBindingAddress,
        > = crate::parser::ParserBindingMetadata::default();
        let mut expanded_binding_uses =
            resolve_module_binding_uses(module.bare_binding_uses(), &module_scope_resolver)
                .map_err(contextualize_access_error)?;
        for resource in module.resources.iter().chain(&selected.resources) {
            let source_file = module
                .parser_binding_metadata
                .resource_source_file(&resource.id);
            let expanded = expand_module_resource(
                resource,
                module
                    .parser_binding_metadata
                    .template_dependency_values(&resource.id),
                &expanded_call,
                &module_scope_resolver,
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
        let mut data_sources: Vec<ModuleResolvedDataSource> = Vec::new();
        for data_source in module.data_sources.iter().chain(&selected.data_sources) {
            let source_file = module
                .parser_binding_metadata
                .resource_source_file(&data_source.id);
            let expanded = expand_module_data_source(
                data_source,
                module
                    .parser_binding_metadata
                    .template_dependency_values(&data_source.id),
                &module_scope_resolver,
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

        let mut compositions: Vec<ModuleResolvedLiveComposition> = module
            .compositions
            .iter()
            .map(|composition| {
                expand_module_composition(
                    composition,
                    &expanded_call,
                    &module_scope_resolver,
                    self.provider_context,
                )
            })
            .collect::<Result<_, _>>()
            .map_err(contextualize_access_error)?;
        for composition in &mut compositions {
            if let Some(report) = settle_prefixed_composition_constraints(
                composition,
                self.records_constraint_reports(),
            )? {
                self.record_constraint_report(report);
            }
        }

        // Every call becomes a composition, including calls with no outputs
        // and anonymous calls. Besides exposing outputs for bound calls, the
        // node is the post-expansion record of the call boundary itself.
        {
            let mut composition_attrs: IndexMap<
                String,
                crate::resource::LiveCompositionAttribute<
                    ParserPlaceholder,
                    AbsoluteBindingAddress,
                >,
            > = IndexMap::new();

            // Copy attribute values from the module definition.
            // Each value is classified into `Forwarded` (single-hop
            // alias) vs `Derived` (multi-source expression) via
            // `LiveCompositionAttribute::from_value` (#3294).
            for attr_param in &module.attribute_params {
                if let Some(value) = &attr_param.value {
                    let source_argument = source_argument_name(value, &argument_values);
                    let declared_type = attr_param
                        .static_type
                        .as_ref()
                        .map(|type_expr| P::resolve_static_type(type_expr, self.config()))
                        .or_else(|| {
                            source_argument.as_deref().and_then(|source_argument| {
                                module
                                    .arguments
                                    .iter()
                                    .find(|argument| argument.name == source_argument)
                                    .map(|argument| {
                                        P::resolve_static_type(&argument.static_type, self.config())
                                    })
                            })
                        });
                    let substituted =
                        resolve_module_value(value, &module_scope_resolver).map_err(|error| {
                            contextualize_access_error(
                                error.with_source_file(attr_param.source_file()),
                            )
                        })?;
                    composition_attrs.insert(
                        attr_param.name.clone(),
                        crate::resource::LiveCompositionAttribute::from_value_with_static_type(
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
                crate::resource::LiveCompositionArgument<ParserPlaceholder, AbsoluteBindingAddress>,
            > = IndexMap::new();
            for arg in &module.arguments {
                if let Some(argument) = resolved_arguments.get(&arg.name) {
                    let static_type = P::resolve_static_type(&arg.static_type, self.config());
                    let recorded = match argument.value_ref() {
                        StructuralOperandRef::Value(value) => {
                            crate::resource::LiveCompositionArgument::from_value_with_static_type(
                                value.clone(),
                                static_type.clone(),
                                argument.origin,
                            )
                        }
                        StructuralOperandRef::Handle {
                            handle,
                            access_base,
                        } => {
                            crate::resource::LiveCompositionArgument::from_handle_with_static_type(
                                access_base.clone(),
                                static_type.clone(),
                                handle.clone(),
                                argument.origin,
                            )
                        }
                        StructuralOperandRef::Parameter {
                            name: _,
                            access_base,
                        } => crate::resource::LiveCompositionArgument::from_value_with_static_type(
                            access_base.clone(),
                            static_type.clone(),
                            argument.origin,
                        ),
                    };
                    signature_arguments.insert(arg.name.clone(), recorded);
                }
            }

            let composition = crate::resource::LiveComposition {
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
                call_path: expanded_call_path.clone(),
                quoted_string_attrs: HashSet::new(),
            };
            compositions.push(composition);
        }

        // Propagate the module's `wait` declarations through the same typed
        // module-scope conversion so a module-internal wait reaches the
        // caller's plan (carina#3061).
        let expanded_wait_bindings: Vec<crate::parser::WaitBinding<AbsoluteBindingAddress>> =
            module
                .wait_bindings
                .iter()
                .filter_map(|wait| resolve_wait_binding(wait, &module_scope_resolver).transpose())
                .collect::<Result<_, _>>()
                .map_err(contextualize_access_error)?;

        // carina#3126: propagate the module's deferred for-expressions
        // (previously dropped at this boundary). Each entry is
        // instance-prefixed the same way `module.resources` are, so
        // the loop resolves against the prefixed module bindings and
        // its generated resources are isolated per module instance
        // (carina#3126 PR-B).
        let expanded_parsed_deferred_for_expressions: Vec<
            ParsedDeferredForExpression<ParserPlaceholder, AbsoluteBindingAddress>,
        > = module
            .parsed_deferred_for_expressions()
            .iter()
            .map(|deferred| {
                let (source_file, dependency_values) = match &deferred.body {
                    DeferredForBody::Resource(resource) => (
                        module
                            .parser_binding_metadata
                            .resource_source_file(&resource.id),
                        module
                            .parser_binding_metadata
                            .template_dependency_values(&resource.id),
                    ),
                    DeferredForBody::Read(data_source) => (
                        module
                            .parser_binding_metadata
                            .resource_source_file(&data_source.id),
                        module
                            .parser_binding_metadata
                            .template_dependency_values(&data_source.id),
                    ),
                    DeferredForBody::ModuleCall(_) => (None, None),
                };
                expand_parsed_deferred_for_expression(
                    deferred,
                    dependency_values,
                    &expanded_call,
                    &module_scope_resolver,
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
        let mut nested_trace = crate::resource::ExpansionTrace::new();
        for nested in &nested_contributions {
            for (leaf, call_sites) in nested.expansion_trace.iter() {
                nested_trace.record(leaf.clone(), call_sites.clone());
            }
        }
        let expansion_trace = build_expansion_trace(
            instance_prefix,
            source_path,
            &nested_trace,
            &resources,
            &data_sources,
        );

        // A module instance contributes only its instance-prefixed runtime
        // graph. Module-local declarations and boundary inputs remain empty
        // on `ParsedFile::default()` because they were consumed above or are
        // rejected while loading the module.
        let mut contribution =
            crate::parser::RootBindingResolvedParsedFile::empty_with_parsed_binding_uses(
                expanded_binding_uses,
            );
        contribution.resources = resources;
        contribution.data_sources = data_sources;
        contribution.compositions = compositions;
        contribution.wait_bindings = expanded_wait_bindings;
        *contribution.parsed_deferred_for_expressions_mut() =
            expanded_parsed_deferred_for_expressions;
        contribution.parser_binding_metadata = expanded_parser_metadata;
        contribution.deferred_module_ifs = selected_module_ifs.forwarded.clone();
        contribution.warnings = module.warnings.clone();
        // Recursive expansion materializes child contributions separately.
        // Prefix them in authored order so the flattened runtime graph keeps
        // origin/main's child-before-parent traversal order. Reversing here is
        // only an assembly detail: each `merge_from` appends its source.
        for mut nested in nested_contributions.into_iter().rev() {
            nested.merge_from(contribution);
            contribution = nested;
        }
        contribution.expansion_trace = expansion_trace;
        reject_dropped_conditional_references(&selected_module_ifs, &contribution)?;
        Ok(contribution)
    }

    fn nested_module_definition<'a>(
        &'a self,
        owner: &ParsedFile,
        owner_directory: Option<&std::path::Path>,
        call: &ModuleCall,
    ) -> Result<(&'a ParsedFile, String, std::path::PathBuf), ModuleError> {
        let import = owner
            .uses
            .iter()
            .find(|import| import.alias == call.module_name)
            .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?;
        let source = std::path::Path::new(&import.path);
        let directory = if source.is_absolute() {
            source.to_path_buf()
        } else {
            owner_directory.unwrap_or(&self.base_dir).join(source)
        };
        let canonical = directory.canonicalize()?;
        let module = self
            .module_cache
            .get(&canonical)
            .ok_or_else(|| ModuleError::UnknownModule(call.module_name.clone()))?;
        Ok((module, import.path.clone(), canonical))
    }

    /// Unit tests exercise isolated expansion mechanics with synthetic module
    /// maps. Production has no witness-free entry point.
    #[cfg(test)]
    pub(super) fn expand_module_call(
        &self,
        call: &crate::parser::ModuleCall,
        instance_prefix: &str,
    ) -> Result<crate::parser::RootBindingResolvedParsedFile, ModuleError> {
        let root = crate::resource::RootScope::new();
        let expanded_call = call
            .clone()
            .resolve_bindings(&mut |binding| root.resolve(binding));
        self.expand_module_call_checked(
            call,
            &expanded_call,
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
fn source_argument_name<B: BindingAddress, C: BindingAddress>(
    value: &crate::resource::Value<ParserPlaceholder, B>,
    argument_values: &HashMap<String, crate::resource::Value<ParserPlaceholder, C>>,
) -> Option<String> {
    use crate::resource::{DeferredValue, Value};

    let candidate = crate::resource::module_argument_reference_path(value)
        .map(|path| path.as_ref().binding().to_string())
        .or_else(|| match value {
            Value::Deferred(DeferredValue::BindingRef { binding }) => Some(binding.to_string()),
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                Some(path.binding().to_string())
            }
            Value::Concrete(
                crate::resource::ConcreteValue::String(_)
                | crate::resource::ConcreteValue::EnumIdentifier(_)
                | crate::resource::ConcreteValue::CanonicalEnum(_)
                | crate::resource::ConcreteValue::Int(_)
                | crate::resource::ConcreteValue::Float(_)
                | crate::resource::ConcreteValue::Bool(_)
                | crate::resource::ConcreteValue::Duration(_)
                | crate::resource::ConcreteValue::List(_)
                | crate::resource::ConcreteValue::StringList(_)
                | crate::resource::ConcreteValue::Map(_),
            )
            | Value::Deferred(
                DeferredValue::ModuleIf(_)
                | DeferredValue::Coalesce {
                    value: _,
                    default: _,
                }
                | DeferredValue::IntegerArithmetic(_)
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::ResolvedFunctionCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::Unavailable {
                    name: _,
                    args: _,
                    reason: _,
                    source_line: _,
                }
                | DeferredValue::Access(_)
                | DeferredValue::Secret(_)
                | DeferredValue::RedactedSecret { hash: _ }
                | DeferredValue::Unknown(
                    ApplyTimeUnknown::UpstreamExport { path: _ }
                    | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                    | ApplyTimeUnknown::PendingReRead(_),
                )
                | DeferredValue::Placeholder(
                    ParserPlaceholder::FnParam { name: _ }
                    | ParserPlaceholder::ModuleArgument {
                        name: _,
                        diagnostic: _,
                    }
                    | ParserPlaceholder::FnLocal { name: _ }
                    | ParserPlaceholder::Loop(LoopSlot::Key)
                    | ParserPlaceholder::Loop(LoopSlot::Index)
                    | ParserPlaceholder::Loop(LoopSlot::Value)
                    | ParserPlaceholder::EmptyInterpolation { source: _ },
                ),
            ) => None,
        })?;
    argument_values
        .contains_key(&candidate)
        .then_some(candidate)
}

fn resolve_nested_module_call(
    call: &ModuleCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<
    crate::parser::ModuleCall<ParserPlaceholder, AbsoluteBindingAddress>,
    ArgumentSubstitutionError,
> {
    let mut resolve_binding =
        |binding: RelativeBindingName| resolver.resolve_declared_binding(&binding);
    let mut resolve_argument = |argument: ModuleArgument| match argument.value_ref() {
        StructuralOperandRef::Value(value) => {
            let forwarded_name = match value {
                Value::Deferred(DeferredValue::BindingRef { binding }) => Some(binding.as_str()),
                Value::Deferred(DeferredValue::Placeholder(
                    ParserPlaceholder::ModuleArgument {
                        name,
                        diagnostic: _,
                    },
                )) => Some(name.as_str()),
                Value::Concrete(
                    crate::resource::ConcreteValue::String(_)
                    | crate::resource::ConcreteValue::EnumIdentifier(_)
                    | crate::resource::ConcreteValue::CanonicalEnum(_)
                    | crate::resource::ConcreteValue::Int(_)
                    | crate::resource::ConcreteValue::Float(_)
                    | crate::resource::ConcreteValue::Bool(_)
                    | crate::resource::ConcreteValue::Duration(_)
                    | crate::resource::ConcreteValue::List(_)
                    | crate::resource::ConcreteValue::StringList(_)
                    | crate::resource::ConcreteValue::Map(_),
                )
                | Value::Deferred(
                    DeferredValue::ModuleIf(_)
                    | DeferredValue::ResourceRef { path: _ }
                    | DeferredValue::Coalesce {
                        value: _,
                        default: _,
                    }
                    | DeferredValue::IntegerArithmetic(_)
                    | DeferredValue::Interpolation(_)
                    | DeferredValue::FunctionCall {
                        name: _,
                        args: _,
                        source_line: _,
                    }
                    | DeferredValue::ResolvedFunctionCall {
                        function: _,
                        functions: _,
                        args: _,
                        source_line: _,
                    }
                    | DeferredValue::Unavailable {
                        name: _,
                        args: _,
                        reason: _,
                        source_line: _,
                    }
                    | DeferredValue::Access(_)
                    | DeferredValue::Secret(_)
                    | DeferredValue::RedactedSecret { hash: _ }
                    | DeferredValue::Unknown(
                        ApplyTimeUnknown::UpstreamExport { path: _ }
                        | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                        | ApplyTimeUnknown::PendingReRead(_),
                    )
                    | DeferredValue::Placeholder(
                        ParserPlaceholder::FnParam { name: _ }
                        | ParserPlaceholder::FnLocal { name: _ }
                        | ParserPlaceholder::Loop(LoopSlot::Key)
                        | ParserPlaceholder::Loop(LoopSlot::Index)
                        | ParserPlaceholder::Loop(LoopSlot::Value)
                        | ParserPlaceholder::EmptyInterpolation { source: _ },
                    ),
                ) => None,
            };
            if let Some(binding) = forwarded_name
                && let Some(forwarded) = resolver.arguments.arguments.get(binding)
            {
                match forwarded.value_ref() {
                    StructuralOperandRef::Handle {
                        handle,
                        access_base: _,
                    } => {
                        return Ok(ModuleArgument::from_handle(
                            handle.clone().with_alias(binding),
                        ));
                    }
                    StructuralOperandRef::Parameter {
                        name,
                        access_base: _,
                    } => {
                        return resolver
                            .arguments
                            .arguments
                            .get(name)
                            .map(|argument| argument.argument.clone())
                            .ok_or_else(|| resolver.unknown_binding(name));
                    }
                    StructuralOperandRef::Value(_forwarded_value) => {}
                }
            }
            let value = resolver.resolve_value(value, ArgumentSubstitutionPosition::Value)?;
            let access_base = argument
                .access_base()
                .map(|base| resolver.resolve_value(base, ArgumentSubstitutionPosition::AccessBase))
                .transpose()?;
            Ok(ModuleArgument::from_parts(value, access_base))
        }
        StructuralOperandRef::Handle {
            handle,
            access_base: _,
        } => {
            resolver.resolve_declared_binding(handle.canonical_binding())?;
            let handle = handle
                .clone()
                .resolve_bindings(&mut |binding| resolver.scope.resolve(binding));
            Ok(ModuleArgument::from_handle(handle))
        }
        StructuralOperandRef::Parameter {
            name,
            access_base: _,
        } => {
            let forwarded = resolver
                .arguments
                .arguments
                .get(name)
                .ok_or_else(|| resolver.unknown_binding(name))?;
            match forwarded.value_ref() {
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } => Ok(ModuleArgument::from_handle(handle.clone().with_alias(name))),
                StructuralOperandRef::Value(_)
                | StructuralOperandRef::Parameter {
                    name: _,
                    access_base: _,
                } => Ok(forwarded.argument.clone()),
            }
        }
    };
    call.clone()
        .try_resolve_bindings(&mut resolve_binding, &mut resolve_argument)
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
    leaf_resources: &[ModuleResolvedResource],
    leaf_data_sources: &[ModuleResolvedDataSource],
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
        out.record(
            crate::resource::PersistentId::new(r.id.clone()),
            vec![this_call_site.clone()],
        );
    }
    for d in leaf_data_sources {
        out.record(
            crate::resource::PersistentId::new(d.id.clone()),
            vec![this_call_site.clone()],
        );
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

/// Join a module-instance path to a system-assigned resource identity.
/// Binding addresses never use this string helper; they cross the phase
/// boundary only through [`ModuleInstanceScope`].
fn prefix_resource_identity(instance_prefix: &str, name: &str) -> String {
    format!("{instance_prefix}.{name}")
}

fn resolve_dependency_bindings(
    dependency_bindings: &BTreeSet<RelativeBindingName>,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<BTreeSet<AbsoluteBindingAddress>, ArgumentSubstitutionError> {
    let mut resolved = BTreeSet::new();
    for binding in dependency_bindings {
        if let Some(argument) = resolver.arguments.arguments.get(binding.as_str()) {
            match argument.value_ref() {
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } => {
                    resolved.insert(handle.canonical_binding().clone());
                }
                StructuralOperandRef::Value(value) => {
                    let mut dependencies = HashSet::new();
                    crate::deps::collect_dependencies(value, &mut dependencies);
                    resolved.extend(dependencies);
                }
                StructuralOperandRef::Parameter {
                    name: _,
                    access_base: _,
                } => {
                    // A standalone module has no concrete caller operand yet.
                    // It therefore contributes no concrete dependency edge;
                    // a real outer call will substitute the handle before
                    // this expansion boundary is crossed.
                }
            }
        } else {
            resolved.insert(resolver.resolve_declared_binding(binding)?);
        }
    }
    Ok(resolved)
}

fn resolve_dependency_values<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<BTreeSet<AbsoluteBindingAddress>, ArgumentSubstitutionError> {
    let mut resolved = BTreeSet::new();
    for value in values {
        let value = resolve_module_value(value, resolver)?;
        let mut dependencies = HashSet::new();
        crate::deps::collect_dependencies(&value, &mut dependencies);
        resolved.extend(dependencies);
    }
    Ok(resolved)
}

fn resolve_directive_dependencies(
    dependencies: &[RelativeBindingName],
    diagnostic_sources: &crate::resource::DiagnosticSourceLocations,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<Vec<AbsoluteBindingAddress>, ArgumentSubstitutionError> {
    dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, binding)| {
            resolver
                .resolve_dependency_binding_at(
                    binding,
                    "directives.depends_on",
                    diagnostic_sources.get_owned(index),
                )
                .transpose()
        })
        .collect()
}

fn resolve_wait_binding(
    wb: &WaitBinding,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<Option<crate::parser::WaitBinding<AbsoluteBindingAddress>>, ArgumentSubstitutionError> {
    let WaitBinding {
        binding,
        target,
        until_raw,
        until_predicate,
        timeout_secs,
        depends_on,
        line,
    } = wb;
    let diagnostic_source = Some((None, *line));
    let Some(target) =
        resolver.resolve_dependency_binding_at(target, "wait target", diagnostic_source.clone())?
    else {
        return Ok(None);
    };
    let Some(predicate_root) = resolver.resolve_dependency_binding_at(
        &until_predicate.root,
        "wait predicate root",
        diagnostic_source.clone(),
    )?
    else {
        return Ok(None);
    };
    Ok(Some(crate::parser::WaitBinding {
        binding: resolver.resolve_declared_binding(binding)?,
        target,
        until_raw: until_raw.clone(),
        until_predicate: crate::parser::UntilPredicateAst {
            root: predicate_root,
            attribute_segments: until_predicate.attribute_segments.clone(),
            rhs: resolver.resolve_wait_rhs(&until_predicate.rhs)?,
        },
        timeout_secs: *timeout_secs,
        depends_on: depends_on
            .iter()
            .filter_map(|binding| {
                resolver
                    .resolve_dependency_binding_at(
                        binding,
                        "wait depends_on",
                        diagnostic_source.clone(),
                    )
                    .transpose()
            })
            .collect::<Result<_, _>>()?,
        line: *line,
    }))
}

fn authored_value_reference_path(value: &Value) -> Option<crate::resource::AuthoredReferencePath> {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            Some(crate::resource::ReferencePath::Access(path.clone()))
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            Some(crate::resource::ReferencePath::Binding(binding.clone()))
        }
        Value::Concrete(
            crate::resource::ConcreteValue::String(_)
            | crate::resource::ConcreteValue::EnumIdentifier(_)
            | crate::resource::ConcreteValue::CanonicalEnum(_)
            | crate::resource::ConcreteValue::Int(_)
            | crate::resource::ConcreteValue::Float(_)
            | crate::resource::ConcreteValue::Bool(_)
            | crate::resource::ConcreteValue::Duration(_)
            | crate::resource::ConcreteValue::List(_)
            | crate::resource::ConcreteValue::StringList(_)
            | crate::resource::ConcreteValue::Map(_),
        )
        | Value::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::Coalesce {
                value: _,
                default: _,
            }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall {
                name: _,
                args: _,
                source_line: _,
            }
            | DeferredValue::ResolvedFunctionCall {
                function: _,
                functions: _,
                args: _,
                source_line: _,
            }
            | DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | DeferredValue::Access(_)
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { hash: _ }
            | DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamExport { path: _ }
                | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                | ApplyTimeUnknown::PendingReRead(_),
            )
            | DeferredValue::Placeholder(
                ParserPlaceholder::FnParam { name: _ }
                | ParserPlaceholder::ModuleArgument {
                    name: _,
                    diagnostic: _,
                }
                | ParserPlaceholder::FnLocal { name: _ }
                | ParserPlaceholder::Loop(LoopSlot::Key)
                | ParserPlaceholder::Loop(LoopSlot::Index)
                | ParserPlaceholder::Loop(LoopSlot::Value)
                | ParserPlaceholder::EmptyInterpolation { source: _ },
            ),
        ) => crate::resource::module_argument_reference_path(value),
    }
}

fn authored_argument_reference_path(
    argument: &ModuleArgument,
) -> Option<crate::resource::AuthoredReferencePath> {
    match argument.value_ref() {
        StructuralOperandRef::Value(value) => authored_value_reference_path(value),
        StructuralOperandRef::Handle {
            handle,
            access_base: _,
        } => Some(crate::resource::ReferencePath::Binding(
            handle.canonical_binding().clone(),
        )),
        StructuralOperandRef::Parameter {
            name: _,
            access_base,
        } => authored_value_reference_path(access_base),
    }
}

fn append_reference_path(
    reference: crate::resource::AuthoredReferencePath,
    postfix: &[PathSegment],
) -> crate::resource::AuthoredReferencePath {
    match reference {
        crate::resource::ReferencePath::Access(path) => crate::resource::ReferencePath::Access(
            postfix
                .iter()
                .cloned()
                .fold(path, crate::resource::AccessPath::with_appended_segment),
        ),
        crate::resource::ReferencePath::Binding(binding) => {
            crate::resource::ReferencePath::BindingAccess {
                binding,
                segments: postfix.to_vec(),
            }
        }
        crate::resource::ReferencePath::BindingAccess {
            binding,
            mut segments,
        } => {
            segments.extend_from_slice(postfix);
            crate::resource::ReferencePath::BindingAccess { binding, segments }
        }
    }
}

fn module_parameter_postfix<B: BindingAddress>(
    value: &crate::resource::Value<ParserPlaceholder, B>,
    arguments: &HashMap<String, ResolvedArgument<B>>,
) -> Option<(String, Vec<PathSegment>)> {
    use crate::resource::{DeferredValue, Value};

    fn collect<B: BindingAddress>(
        value: &crate::resource::Value<ParserPlaceholder, B>,
        arguments: &HashMap<String, ResolvedArgument<B>>,
    ) -> Option<(String, Vec<PathSegment>)> {
        match value {
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument {
                name,
                diagnostic: _,
            })) if arguments.contains_key(name) => Some((name.clone(), Vec::new())),
            Value::Deferred(DeferredValue::Access(access)) => {
                let (argument, mut postfix) = collect(access.base(), arguments)?;
                postfix.push(access.step().clone());
                Some((argument, postfix))
            }
            Value::Deferred(DeferredValue::ResourceRef { path })
                if arguments.contains_key(path.binding()) =>
            {
                let mut postfix = Vec::with_capacity(path.segments().len() + 1);
                postfix.push(PathSegment::Field {
                    name: path.attribute().to_string(),
                });
                postfix.extend_from_slice(path.segments());
                Some((path.binding().to_string(), postfix))
            }
            Value::Deferred(DeferredValue::BindingRef { binding })
                if arguments.contains_key(binding.as_str()) =>
            {
                Some((binding.as_str().to_string(), Vec::new()))
            }
            Value::Concrete(
                crate::resource::ConcreteValue::String(_)
                | crate::resource::ConcreteValue::EnumIdentifier(_)
                | crate::resource::ConcreteValue::CanonicalEnum(_)
                | crate::resource::ConcreteValue::Int(_)
                | crate::resource::ConcreteValue::Float(_)
                | crate::resource::ConcreteValue::Bool(_)
                | crate::resource::ConcreteValue::Duration(_)
                | crate::resource::ConcreteValue::List(_)
                | crate::resource::ConcreteValue::StringList(_)
                | crate::resource::ConcreteValue::Map(_),
            )
            | Value::Deferred(
                DeferredValue::ModuleIf(_)
                | DeferredValue::ResourceRef { path: _ }
                | DeferredValue::BindingRef { binding: _ }
                | DeferredValue::Coalesce {
                    value: _,
                    default: _,
                }
                | DeferredValue::IntegerArithmetic(_)
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::ResolvedFunctionCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::Unavailable {
                    name: _,
                    args: _,
                    reason: _,
                    source_line: _,
                }
                | DeferredValue::Secret(_)
                | DeferredValue::RedactedSecret { hash: _ }
                | DeferredValue::Unknown(
                    ApplyTimeUnknown::UpstreamExport { path: _ }
                    | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                    | ApplyTimeUnknown::PendingReRead(_),
                )
                | DeferredValue::Placeholder(
                    ParserPlaceholder::FnParam { name: _ }
                    | ParserPlaceholder::ModuleArgument {
                        name: _,
                        diagnostic: _,
                    }
                    | ParserPlaceholder::FnLocal { name: _ }
                    | ParserPlaceholder::Loop(LoopSlot::Key)
                    | ParserPlaceholder::Loop(LoopSlot::Index)
                    | ParserPlaceholder::Loop(LoopSlot::Value)
                    | ParserPlaceholder::EmptyInterpolation { source: _ },
                ),
            ) => None,
        }
    }

    collect(value, arguments)
}

#[derive(Clone)]
struct ResolvedArgument<B: BindingAddress = AbsoluteBindingAddress> {
    argument: ModuleArgument<ParserPlaceholder, B>,
    origin: ArgumentOrigin,
    caller_reference: Option<crate::resource::AuthoredReferencePath>,
    forwarded_from_enclosing_parameter: bool,
}

impl<B: BindingAddress> std::ops::Deref for ResolvedArgument<B> {
    type Target = ModuleArgument<ParserPlaceholder, B>;

    fn deref(&self) -> &Self::Target {
        &self.argument
    }
}

impl<B: BindingAddress> ResolvedArgument<B> {
    fn source_line(&self, default_line: usize) -> usize {
        match self.origin {
            ArgumentOrigin::Authored {
                key_span: _,
                value_span,
            } => value_span.start_line(),
            ArgumentOrigin::Defaulted => default_line,
        }
    }

    fn substitution(
        &self,
        name: &str,
        position: ArgumentSubstitutionPosition,
    ) -> Result<StructuralOperandRef<'_, ParserPlaceholder, B>, String> {
        match (position, self.value_ref()) {
            (ArgumentSubstitutionPosition::Value, StructuralOperandRef::Value(value)) => {
                Ok(StructuralOperandRef::Value(value))
            }
            (
                ArgumentSubstitutionPosition::Value,
                StructuralOperandRef::Handle {
                    handle: _,
                    access_base: _,
                },
            ) => Err(BareStructuralHandle {
                binding: name.to_string(),
                kind: StructuralHandleKind::ModuleParameter,
            }
            .to_string()),
            (
                ArgumentSubstitutionPosition::Value,
                operand @ StructuralOperandRef::Parameter {
                    name: _,
                    access_base: _,
                },
            ) => Ok(operand),
            (ArgumentSubstitutionPosition::AccessBase, StructuralOperandRef::Value(value)) => {
                Ok(StructuralOperandRef::Value(value))
            }
            (
                ArgumentSubstitutionPosition::AccessBase,
                StructuralOperandRef::Handle {
                    handle,
                    access_base,
                },
            ) => Ok(StructuralOperandRef::Handle {
                handle,
                access_base,
            }),
            (
                ArgumentSubstitutionPosition::AccessBase,
                StructuralOperandRef::Parameter { name, access_base },
            ) => Ok(StructuralOperandRef::Parameter { name, access_base }),
        }
    }
}

struct ArgumentSubstitutions<'a, B: BindingAddress = AbsoluteBindingAddress> {
    arguments: &'a HashMap<String, ResolvedArgument<B>>,
    default_line: usize,
    caller_source_file: &'a std::path::Path,
    binding_index: Option<&'a BindingIndex<'a>>,
    config: &'a crate::parser::ProviderContext,
}

impl<B: BindingAddress> Copy for ArgumentSubstitutions<'_, B> {}

impl<B: BindingAddress> Clone for ArgumentSubstitutions<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

struct ModuleScopeResolver<'a> {
    scope: &'a ModuleInstanceScope,
    local_bindings: &'a HashSet<RelativeBindingName>,
    arguments: ArgumentSubstitutions<'a>,
    call_path: &'a crate::resource::ExpandedCompositionCallPath,
    argument_placeholders: HashMap<String, ResolvedArgument<RelativeBindingName>>,
}

impl ModuleScopeResolver<'_> {
    fn resolve_declared_binding(
        &self,
        binding: &RelativeBindingName,
    ) -> Result<AbsoluteBindingAddress, ArgumentSubstitutionError> {
        self.resolve_declared_binding_at(binding, None)
    }

    fn resolve_declared_binding_at(
        &self,
        binding: &RelativeBindingName,
        diagnostic_source: Option<(Option<std::path::PathBuf>, usize)>,
    ) -> Result<AbsoluteBindingAddress, ArgumentSubstitutionError> {
        if self.local_bindings.contains(binding) {
            Ok(self.scope.resolve(binding.clone()))
        } else {
            Err(self.unknown_binding_at(binding.as_str(), diagnostic_source))
        }
    }

    fn resolve_dependency_binding_at(
        &self,
        binding: &RelativeBindingName,
        context: &str,
        diagnostic_source: Option<(Option<std::path::PathBuf>, usize)>,
    ) -> Result<Option<AbsoluteBindingAddress>, ArgumentSubstitutionError> {
        if let Some(argument) = self.arguments.arguments.get(binding.as_str()) {
            return match argument.value_ref() {
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } if handle.kind().is_dependency_target() => {
                    Ok(Some(handle.canonical_binding().clone()))
                }
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } => Err(ArgumentSubstitutionError {
                    source_file: Some(self.arguments.caller_source_file.to_path_buf()),
                    line: argument.source_line(self.arguments.default_line),
                    message: format!(
                        "{context}: '{}' is {} and cannot be used as a dependency target",
                        handle
                            .alias_name()
                            .unwrap_or_else(|| handle.canonical_binding_name()),
                        handle.kind().description(),
                    ),
                    parse_error: None,
                }),
                StructuralOperandRef::Value(_) => Err(ArgumentSubstitutionError {
                    source_file: Some(self.arguments.caller_source_file.to_path_buf()),
                    line: argument.source_line(self.arguments.default_line),
                    message: format!(
                        "{context} parameter '{binding}' received a value, not a resource handle"
                    ),
                    parse_error: None,
                }),
                StructuralOperandRef::Parameter {
                    name: _,
                    access_base: _,
                } => Ok(None),
            };
        }
        self.resolve_declared_binding_at(binding, diagnostic_source)
            .map(Some)
    }

    fn resolve_value(
        &self,
        value: &Value,
        position: ArgumentSubstitutionPosition,
    ) -> Result<ModuleResolvedValue, ArgumentSubstitutionError> {
        let normalized = substitute_arguments_with_position(
            value,
            ArgumentSubstitutions {
                arguments: &self.argument_placeholders,
                default_line: self.arguments.default_line,
                caller_source_file: self.arguments.caller_source_file,
                binding_index: None,
                config: self.arguments.config,
            },
            position,
        )?;
        for binding in normalized.root_bindings() {
            if !self.local_bindings.contains(&binding) {
                let diagnostic_source = normalized.diagnostic_source_for_root_binding(&binding);
                return Err(self.unknown_binding_at(binding.as_str(), diagnostic_source));
            }
        }
        let expanded = normalized.resolve_module(self.scope);
        let substituted = substitute_arguments_with_position(&expanded, self.arguments, position)?;
        let evaluated = evaluate_expanded_module_functions(&substituted, self.arguments.config)?;
        select_expanded_module_ifs(&evaluated)
    }

    fn resolve_wait_rhs(
        &self,
        value: &crate::resource::Value<crate::resource::NoPlaceholder, RelativeBindingName>,
    ) -> Result<
        crate::resource::Value<crate::resource::NoPlaceholder, AbsoluteBindingAddress>,
        ArgumentSubstitutionError,
    > {
        for binding in value.root_bindings() {
            if !self.local_bindings.contains(&binding) {
                let diagnostic_source = value.diagnostic_source_for_root_binding(&binding);
                return Err(self.unknown_binding_at(binding.as_str(), diagnostic_source));
            }
        }
        Ok(value.clone().resolve_module(self.scope))
    }

    fn resolve_structural_handle(
        &self,
        handle: &crate::eval_value::StructuralHandle<RelativeBindingName>,
    ) -> Result<
        crate::eval_value::StructuralHandle<AbsoluteBindingAddress>,
        ArgumentSubstitutionError,
    > {
        let binding = handle.canonical_binding();
        if let Some(argument) = self.arguments.arguments.get(binding.as_str()) {
            return match argument.value_ref() {
                StructuralOperandRef::Handle {
                    handle,
                    access_base: _,
                } => Ok(handle.clone()),
                StructuralOperandRef::Value(_) => Err(ArgumentSubstitutionError {
                    source_file: Some(self.arguments.caller_source_file.to_path_buf()),
                    line: argument.source_line(self.arguments.default_line),
                    message: format!(
                        "structural parameter '{binding}' received a value, not a handle"
                    ),
                    parse_error: None,
                }),
                StructuralOperandRef::Parameter {
                    name,
                    access_base: _,
                } => Err(ArgumentSubstitutionError {
                    source_file: Some(self.arguments.caller_source_file.to_path_buf()),
                    line: argument.source_line(self.arguments.default_line),
                    message: format!("unresolved forwarded module parameter '{name}'"),
                    parse_error: None,
                }),
            };
        }
        self.resolve_declared_binding(binding)?;
        Ok(handle
            .clone()
            .resolve_bindings(&mut |binding| self.scope.resolve(binding)))
    }

    fn unknown_binding(&self, binding: &str) -> ArgumentSubstitutionError {
        self.unknown_binding_at(binding, None)
    }

    fn unknown_binding_at(
        &self,
        binding: &str,
        diagnostic_source: Option<(Option<std::path::PathBuf>, usize)>,
    ) -> ArgumentSubstitutionError {
        let (source_file, line) = diagnostic_source.unwrap_or((None, self.arguments.default_line));
        ArgumentSubstitutionError {
            source_file,
            line,
            message: format!(
                "unknown binding '{binding}' in {}",
                self.scope.description()
            ),
            parse_error: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ConditionalBindingOrigin {
    condition: String,
    source_file: std::path::PathBuf,
    source_line: usize,
}

#[derive(Debug)]
struct SelectedDeferredIfs {
    contribution: ParsedFile,
    forwarded: Vec<DeferredStructuralIf<ParserPlaceholder, AbsoluteBindingAddress>>,
    dropped: HashMap<AbsoluteBindingAddress, (RelativeBindingName, ConditionalBindingOrigin)>,
}

fn select_deferred_module_ifs(
    module: &ParsedFile,
    module_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<SelectedDeferredIfs, ArgumentSubstitutionError> {
    let mut origins = HashMap::<RelativeBindingName, ConditionalBindingOrigin>::new();
    for deferred in &module.deferred_module_ifs {
        let origin = ConditionalBindingOrigin {
            condition: deferred.condition().source_expression().to_string(),
            source_file: deferred.condition().source_file().to_path_buf(),
            source_line: deferred.condition().source_line(),
        };
        for branch in deferred.branches() {
            if let Some(binding) = branch.binding_address() {
                origins.insert(binding.clone(), origin.clone());
            }
        }
    }

    let mut contribution = ParsedFile::default();
    let mut forwarded = Vec::new();
    let mut selected_bindings = HashSet::new();
    let mut nested_dropped = HashMap::new();
    for deferred in &module.deferred_module_ifs {
        let condition = resolve_module_value(deferred.condition().value(), resolver)
            .map_err(|error| error.with_source_file(deferred.condition().source_file()))?;
        let decision = decide_expanded_if_condition(&condition).map_err(|message| {
            ArgumentSubstitutionError {
                source_file: Some(deferred.condition().source_file().to_path_buf()),
                line: deferred.condition().source_line(),
                message,
                parse_error: None,
            }
        })?;

        let selected = match decision {
            Some(true) => Some(deferred.then_branch()),
            Some(false) => deferred.else_branch(),
            None => {
                selected_bindings.extend(
                    deferred
                        .branches()
                        .filter_map(DeferredIfBody::binding_address)
                        .cloned(),
                );
                let condition = DeferredIfCondition::new_with_source_expression(
                    condition,
                    deferred.condition().source_file().to_path_buf(),
                    deferred.condition().source_line(),
                    deferred.condition().source_expression().to_string(),
                );
                let then_branch =
                    expand_forwarded_if_body(deferred.then_branch(), module_call, resolver)?;
                let else_branch = deferred
                    .else_branch()
                    .map(|branch| expand_forwarded_if_body(branch, module_call, resolver))
                    .transpose()?;
                forwarded.push(DeferredStructuralIf::new(
                    condition,
                    then_branch,
                    else_branch,
                ));
                None
            }
        };

        let Some(selected) = selected else {
            continue;
        };
        if let Some(binding) = selected.binding_address() {
            selected_bindings.insert(binding.clone());
        }
        match selected {
            DeferredIfBody::Resource(resource) => {
                contribution.resources.push((**resource).clone());
            }
            DeferredIfBody::DataSource(data_source) => {
                contribution.data_sources.push((**data_source).clone());
            }
            DeferredIfBody::ModuleCall(call) => contribution.module_calls.push(call.clone()),
            DeferredIfBody::ExpandedModuleCall(prepared) => {
                let mut prepared = prepared.as_ref().clone();
                let nested = select_deferred_module_ifs(&prepared, module_call, resolver)?;
                prepared.deferred_module_ifs.clear();
                prepared.merge_from(nested.contribution);
                contribution.merge_from(prepared);
                forwarded.extend(nested.forwarded);
                nested_dropped.extend(nested.dropped);
            }
        }
    }

    let mut dropped = origins
        .into_iter()
        .filter(|(binding, _origin)| !selected_bindings.contains(binding))
        .map(|(binding, origin)| {
            let absolute = resolver.scope.resolve(binding.clone());
            (absolute, (binding, origin))
        })
        .collect::<HashMap<_, _>>();
    dropped.extend(nested_dropped);
    Ok(SelectedDeferredIfs {
        contribution,
        forwarded,
        dropped,
    })
}

fn expand_forwarded_if_body(
    body: &DeferredIfBody,
    module_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<DeferredIfBody<ParserPlaceholder, AbsoluteBindingAddress>, ArgumentSubstitutionError> {
    match body {
        DeferredIfBody::Resource(resource) => Ok(DeferredIfBody::Resource(Box::new(
            expand_module_resource(resource, None, module_call, resolver)?,
        ))),
        DeferredIfBody::DataSource(data_source) => Ok(DeferredIfBody::DataSource(Box::new(
            expand_module_data_source(data_source, None, resolver)?,
        ))),
        DeferredIfBody::ModuleCall(call) => Ok(DeferredIfBody::ModuleCall(
            resolve_nested_module_call(call, resolver)?,
        )),
        DeferredIfBody::ExpandedModuleCall(contribution) => {
            let contribution = contribution
                .as_ref()
                .clone()
                .resolve_bindings(&mut |binding| resolver.scope.resolve(binding));
            Ok(DeferredIfBody::ExpandedModuleCall(std::sync::Arc::new(
                contribution,
            )))
        }
    }
}

fn reject_dropped_conditional_references(
    selected: &SelectedDeferredIfs,
    expanded: &crate::parser::RootBindingResolvedParsedFile,
) -> Result<(), ModuleError> {
    if selected.dropped.is_empty() {
        return Ok(());
    }

    let dropped_value = |value: &ModuleResolvedValue| {
        value
            .root_bindings()
            .into_iter()
            .find_map(|binding| selected.dropped.get(&binding))
    };
    let dropped_binding = |binding: &AbsoluteBindingAddress| selected.dropped.get(binding);
    let mut found = None;

    for resource in &expanded.resources {
        found = resource
            .attributes
            .values()
            .find_map(&dropped_value)
            .or_else(|| {
                resource
                    .dependency_bindings
                    .iter()
                    .find_map(&dropped_binding)
            })
            .or_else(|| {
                resource
                    .directives
                    .depends_on
                    .iter()
                    .find_map(&dropped_binding)
            });
        if found.is_some() {
            break;
        }
    }
    if found.is_none() {
        for data_source in &expanded.data_sources {
            found = data_source
                .attributes
                .values()
                .find_map(&dropped_value)
                .or_else(|| {
                    data_source
                        .dependency_bindings
                        .iter()
                        .find_map(&dropped_binding)
                })
                .or_else(|| {
                    data_source
                        .directives
                        .depends_on
                        .iter()
                        .find_map(&dropped_binding)
                });
            if found.is_some() {
                break;
            }
        }
    }
    if found.is_none() {
        for composition in &expanded.compositions {
            found = composition
                .signature
                .arguments
                .values()
                .find_map(|argument| dropped_value(argument.value()))
                .or_else(|| {
                    composition
                        .signature
                        .attributes
                        .values()
                        .find_map(|attribute| dropped_value(&attribute.to_value()))
                })
                .or_else(|| {
                    composition
                        .dependency_bindings
                        .iter()
                        .find_map(&dropped_binding)
                });
            if found.is_some() {
                break;
            }
        }
    }
    if found.is_none() {
        for wait in &expanded.wait_bindings {
            found = dropped_binding(&wait.target)
                .or_else(|| dropped_binding(&wait.until_predicate.root))
                .or_else(|| wait.depends_on.iter().find_map(&dropped_binding))
                .or_else(|| {
                    wait.until_predicate
                        .rhs
                        .root_bindings()
                        .into_iter()
                        .find_map(|binding| selected.dropped.get(&binding))
                });
            if found.is_some() {
                break;
            }
        }
    }
    if found.is_none() {
        for deferred in expanded.parsed_deferred_for_expressions() {
            found = dropped_value(&deferred.iterable);
            if found.is_none() {
                found = match &deferred.body {
                    DeferredForBody::Resource(resource) => {
                        resource.attributes.values().find_map(&dropped_value)
                    }
                    DeferredForBody::Read(data_source) => {
                        data_source.attributes.values().find_map(&dropped_value)
                    }
                    DeferredForBody::ModuleCall(call) => {
                        let mut referenced = None;
                        let _ = call.try_visit_values_for_module_if_audit(&mut |value| {
                            if let Some(binding) = dropped_value(value) {
                                referenced = Some(binding);
                                Err(())
                            } else {
                                Ok(())
                            }
                        });
                        referenced
                    }
                };
            }
            if found.is_some() {
                break;
            }
        }
    }
    if found.is_none() {
        let _ = expanded
            .parser_binding_metadata
            .try_visit_values(&mut |value| {
                if let Some(binding) = dropped_value(value) {
                    found = Some(binding);
                    Err(())
                } else {
                    Ok(())
                }
            });
    }
    if found.is_none() {
        let _ = expanded.try_visit_bare_binding_values(&mut |value| {
            if let Some(binding) = dropped_value(value) {
                found = Some(binding);
                Err(())
            } else {
                Ok(())
            }
        });
    }

    let Some((binding, origin)) = found else {
        return Ok(());
    };
    Err(ModuleError::DroppedConditionalBinding {
        binding: binding.to_string(),
        condition: origin.condition.clone(),
        source_path: origin.source_file.clone(),
        line: origin.source_line,
    })
}

fn resolve_module_binding_uses(
    uses: &ParsedBindingUses,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<ParsedBindingUses<AbsoluteBindingAddress>, ArgumentSubstitutionError> {
    let mut resolved = ParsedBindingUses::default();
    for record in uses.records() {
        let source = match &record.source {
            BareBindingSource::Value {
                value,
                declared_type,
            } => {
                let parameter = if let Value::Deferred(DeferredValue::Placeholder(
                    ParserPlaceholder::ModuleArgument {
                        name,
                        diagnostic: _,
                    },
                )) = value.as_ref()
                {
                    resolver.arguments.arguments.get(name)
                } else {
                    None
                };
                match parameter.map(|argument| argument.value_ref()) {
                    Some(StructuralOperandRef::Value(value)) => BareBindingSource::Value {
                        value: std::sync::Arc::new(value.clone()),
                        declared_type: declared_type.clone(),
                    },
                    Some(StructuralOperandRef::Handle {
                        handle,
                        access_base: _,
                    }) => BareBindingSource::Structural(
                        handle.clone().with_alias(record.authored.name.clone()),
                    ),
                    Some(StructuralOperandRef::Parameter {
                        name: _,
                        access_base,
                    }) => BareBindingSource::Value {
                        value: std::sync::Arc::new(access_base.clone()),
                        declared_type: declared_type.clone(),
                    },
                    None => BareBindingSource::Value {
                        value: std::sync::Arc::new(resolve_module_value(value, resolver)?),
                        declared_type: declared_type.clone(),
                    },
                }
            }
            BareBindingSource::Structural(handle) => match resolver
                .arguments
                .arguments
                .get(handle.canonical_binding().as_str())
                .map(|argument| argument.value_ref())
            {
                Some(StructuralOperandRef::Parameter {
                    name: _,
                    access_base,
                }) => BareBindingSource::Value {
                    value: std::sync::Arc::new(access_base.clone()),
                    declared_type: None,
                },
                Some(
                    StructuralOperandRef::Value(_)
                    | StructuralOperandRef::Handle {
                        handle: _,
                        access_base: _,
                    },
                )
                | None => {
                    BareBindingSource::Structural(resolver.resolve_structural_handle(handle)?)
                }
            },
        };
        resolved.record(record.authored.clone(), source, record.pending_slot.clone());
    }
    Ok(resolved)
}

#[derive(Clone, Copy)]
enum ArgumentSubstitutionPosition {
    Value,
    AccessBase,
}

fn bind_module_constraints(
    module: &ParsedFile,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings<ParserPlaceholder>>,
) -> Result<
    Vec<crate::resource::PendingModuleConstraint<ParserPlaceholder, AbsoluteBindingAddress>>,
    ArgumentSubstitutionError,
> {
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
    expression: &crate::parser::ValidateExpr<ParserPlaceholder, RelativeBindingName>,
    arguments: ArgumentSubstitutions<'_>,
    resolved_bindings: Option<&crate::binding_index::ResolvedBindings<ParserPlaceholder>>,
    line: usize,
    source_file: &std::path::Path,
) -> Result<
    crate::parser::ValidateExpr<ParserPlaceholder, AbsoluteBindingAddress>,
    ArgumentSubstitutionError,
> {
    use crate::parser::ValidateExpr;

    let bind_value = |argument: &str,
                      position: ArgumentSubstitutionPosition,
                      path: &[PathSegment]|
     -> Result<
        ValidateExpr<ParserPlaceholder, AbsoluteBindingAddress>,
        ArgumentSubstitutionError,
    > {
        let Some(resolved_argument) = arguments.arguments.get(argument) else {
            return Err(ArgumentSubstitutionError {
                source_file: Some(source_file.to_path_buf()),
                line,
                message: format!("unknown module argument '{argument}'"),
                parse_error: None,
            });
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
        ValidateExpr::Var(argument) => match arguments.arguments.contains_key(argument) {
            true => return bind_value(argument, ArgumentSubstitutionPosition::Value, &[]),
            false => ValidateExpr::Var(argument.clone()),
        },
        ValidateExpr::Access { argument, path } => {
            match arguments.arguments.contains_key(argument) {
                true => {
                    return bind_value(argument, ArgumentSubstitutionPosition::AccessBase, path);
                }
                false => ValidateExpr::Access {
                    argument: argument.clone(),
                    path: path.clone(),
                },
            }
        }
        ValidateExpr::BoundArgument { argument, .. } => {
            return Err(ArgumentSubstitutionError {
                source_file: Some(source_file.to_path_buf()),
                line,
                message: format!(
                    "module constraint for '{argument}' crossed its expansion boundary twice"
                ),
                parse_error: None,
            });
        }
        ValidateExpr::Bool(value) => ValidateExpr::Bool(*value),
        ValidateExpr::Int(value) => ValidateExpr::Int(*value),
        ValidateExpr::Float(value) => ValidateExpr::Float(*value),
        ValidateExpr::Duration(value) => ValidateExpr::Duration(*value),
        ValidateExpr::String(value) => ValidateExpr::String(value.clone()),
        ValidateExpr::Null => ValidateExpr::Null,
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
        ValidateExpr::IntegerArithmetic {
            lhs,
            op,
            rhs,
            operator_span,
        } => ValidateExpr::IntegerArithmetic {
            lhs: Box::new(bind_constraint_expression(
                lhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
            op: *op,
            rhs: Box::new(bind_constraint_expression(
                rhs,
                arguments,
                resolved_bindings,
                line,
                source_file,
            )?),
            operator_span: *operator_span,
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

fn resolve_module_value(
    value: &Value,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<ModuleResolvedValue, ArgumentSubstitutionError> {
    let mut resolved = resolver.resolve_value(value, ArgumentSubstitutionPosition::Value)?;
    resolved.canonicalize_in_place();
    Ok(resolved)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpandedIfDependency {
    Static,
    ModuleArgument,
    Runtime,
}

impl ExpandedIfDependency {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Runtime, _) | (_, Self::Runtime) => Self::Runtime,
            (Self::ModuleArgument, _) | (_, Self::ModuleArgument) => Self::ModuleArgument,
            (Self::Static, Self::Static) => Self::Static,
        }
    }
}

fn expanded_if_dependency(value: &ModuleResolvedValue) -> ExpandedIfDependency {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    match value {
        Value::Concrete(ConcreteValue::List(values)) => values
            .iter()
            .fold(ExpandedIfDependency::Static, |dependency, value| {
                dependency.combine(expanded_if_dependency(value))
            }),
        Value::Concrete(ConcreteValue::Map(values)) => values
            .values()
            .fold(ExpandedIfDependency::Static, |dependency, value| {
                dependency.combine(expanded_if_dependency(value))
            }),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        ) => ExpandedIfDependency::Static,
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred
            .values()
            .into_iter()
            .fold(ExpandedIfDependency::Static, |dependency, value| {
                dependency.combine(expanded_if_dependency(value))
            }),
        Value::Deferred(
            DeferredValue::FunctionCall {
                name: _,
                args,
                source_line: _,
            }
            | DeferredValue::ResolvedFunctionCall {
                function: _,
                functions: _,
                args,
                source_line: _,
            },
        ) => args
            .iter()
            .fold(ExpandedIfDependency::Static, |dependency, argument| {
                dependency.combine(expanded_if_dependency(argument.walker_value()))
            }),
        Value::Deferred(DeferredValue::Access(access)) => expanded_if_dependency(access.base()),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().fold(
            ExpandedIfDependency::Static,
            |dependency, part| match part {
                InterpolationPart::Literal(_literal) => dependency,
                InterpolationPart::Expr(value) => dependency.combine(expanded_if_dependency(value)),
            },
        ),
        Value::Deferred(DeferredValue::Secret(inner)) => expanded_if_dependency(inner),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            expanded_if_dependency(value).combine(expanded_if_dependency(default))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            expanded_if_dependency(expression.lhs())
                .combine(expanded_if_dependency(expression.rhs()))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => expanded_unknown_dependency(reason),
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            expanded_placeholder_dependency(placeholder)
        }
        Value::Deferred(
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | DeferredValue::RedactedSecret { hash: _ },
        ) => ExpandedIfDependency::Runtime,
    }
}

fn expanded_unknown_dependency<B: BindingAddress>(
    reason: &ApplyTimeUnknown<B>,
) -> ExpandedIfDependency {
    match reason {
        ApplyTimeUnknown::UpstreamExport { path: _ }
        | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
        | ApplyTimeUnknown::PendingReRead(_) => ExpandedIfDependency::Runtime,
    }
}

fn expanded_placeholder_dependency(placeholder: &ParserPlaceholder) -> ExpandedIfDependency {
    match placeholder {
        ParserPlaceholder::ModuleArgument {
            name: _,
            diagnostic: _,
        } => ExpandedIfDependency::ModuleArgument,
        ParserPlaceholder::FnParam { name: _ }
        | ParserPlaceholder::FnLocal { name: _ }
        | ParserPlaceholder::Loop(LoopSlot::Key)
        | ParserPlaceholder::Loop(LoopSlot::Index)
        | ParserPlaceholder::Loop(LoopSlot::Value)
        | ParserPlaceholder::EmptyInterpolation { source: _ } => ExpandedIfDependency::Runtime,
    }
}

fn decide_expanded_if_condition(value: &ModuleResolvedValue) -> Result<Option<bool>, String> {
    let mut value = value.clone();
    value.canonicalize_in_place();
    if let crate::resource::Value::Concrete(crate::resource::ConcreteValue::Bool(condition)) =
        &value
    {
        return Ok(Some(*condition));
    }
    match expanded_if_dependency(&value) {
        ExpandedIfDependency::ModuleArgument => Ok(None),
        ExpandedIfDependency::Runtime => {
            Err("if condition depends on a runtime value; condition must be statically known at parse time".to_string())
        }
        ExpandedIfDependency::Static => Err(format!(
            "if condition must be a Bool value, got: {}",
            crate::ValueDescription::of(&value)
        )),
    }
}

fn select_expanded_module_ifs(
    value: &ModuleResolvedValue,
) -> Result<ModuleResolvedValue, ArgumentSubstitutionError> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let condition = select_expanded_module_ifs(deferred.condition().value())?;
            match decide_expanded_if_condition(&condition).map_err(|message| {
                ArgumentSubstitutionError {
                    source_file: Some(deferred.condition().source_file().to_path_buf()),
                    line: deferred.condition().source_line(),
                    message,
                    parse_error: None,
                }
            })? {
                Some(true) => select_expanded_module_ifs(deferred.then_value())?,
                Some(false) => select_expanded_module_ifs(deferred.else_value())?,
                None => Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
                    DeferredIfCondition::new_with_source_expression(
                        condition,
                        deferred.condition().source_file().to_path_buf(),
                        deferred.condition().source_line(),
                        deferred.condition().source_expression().to_string(),
                    ),
                    select_expanded_module_ifs(deferred.then_value())?,
                    select_expanded_module_ifs(deferred.else_value())?,
                )))),
            }
        }
        Value::Concrete(ConcreteValue::List(values)) => Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .map(select_expanded_module_ifs)
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(values)) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(name, value)| Ok((name.clone(), select_expanded_module_ifs(value)?)))
                .collect::<Result<_, ArgumentSubstitutionError>>()?,
        )),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(select_expanded_module_ifs(value)?),
                default: Box::new(select_expanded_module_ifs(default)?),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = select_expanded_module_ifs(expression.lhs())?;
            let rhs = select_expanded_module_ifs(expression.rhs())?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    parse_error: None,
                })?
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut selected = Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|part| match part {
                        InterpolationPart::Literal(literal) => {
                            Ok(InterpolationPart::Literal(literal.clone()))
                        }
                        InterpolationPart::Expr(value) => {
                            Ok(InterpolationPart::Expr(select_expanded_module_ifs(value)?))
                        }
                    })
                    .collect::<Result<_, ArgumentSubstitutionError>>()?,
            ));
            selected.canonicalize_in_place();
            selected
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name: *name,
            args: args
                .iter()
                .map(|argument| argument.try_map_value(select_expanded_module_ifs))
                .collect::<Result<_, _>>()?,
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => Value::Deferred(DeferredValue::Unavailable {
            name: *name,
            args: args
                .iter()
                .map(|argument| argument.try_map_value(select_expanded_module_ifs))
                .collect::<Result<_, _>>()?,
            reason: reason.clone(),
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
                .map(|argument| argument.try_map_value(select_expanded_module_ifs))
                .collect::<Result<_, _>>()?,
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                select_expanded_module_ifs(access.base())?,
                AccessMissPolicy::Error,
            )
            .map_err(|error| ArgumentSubstitutionError {
                source_file: None,
                line: access.source_line(),
                message: error.to_string(),
                parse_error: None,
            })?,
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(select_expanded_module_ifs(inner)?),
        )),
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
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::RedactedSecret { hash: _ },
        ) => value.clone(),
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            let _dependency = expanded_unknown_dependency(reason);
            value.clone()
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            let _dependency = expanded_placeholder_dependency(placeholder);
            value.clone()
        }
    })
}

fn evaluate_expanded_module_functions(
    value: &ModuleResolvedValue,
    config: &crate::parser::ProviderContext,
) -> Result<ModuleResolvedValue, ArgumentSubstitutionError> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
                DeferredIfCondition::new_with_source_expression(
                    evaluate_expanded_module_functions(deferred.condition().value(), config)?,
                    deferred.condition().source_file().to_path_buf(),
                    deferred.condition().source_line(),
                    deferred.condition().source_expression().to_string(),
                ),
                evaluate_expanded_module_functions(deferred.then_value(), config)?,
                evaluate_expanded_module_functions(deferred.else_value(), config)?,
            ))))
        }
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(|item| evaluate_expanded_module_functions(item, config))
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(entries)) => Value::Concrete(ConcreteValue::Map(
            entries
                .iter()
                .map(|(name, item)| {
                    Ok((
                        name.clone(),
                        evaluate_expanded_module_functions(item, config)?,
                    ))
                })
                .collect::<Result<_, ArgumentSubstitutionError>>()?,
        )),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(evaluate_expanded_module_functions(value, config)?),
                default: Box::new(evaluate_expanded_module_functions(default, config)?),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = evaluate_expanded_module_functions(expression.lhs(), config)?;
            let rhs = evaluate_expanded_module_functions(expression.rhs(), config)?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: expression.operator_span().line().unwrap_or(0),
                    message: error.to_string(),
                    parse_error: None,
                })?
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|part| match part {
                        InterpolationPart::Literal(literal) => {
                            Ok(InterpolationPart::Literal(literal.clone()))
                        }
                        InterpolationPart::Expr(expression) => Ok(InterpolationPart::Expr(
                            evaluate_expanded_module_functions(expression, config)?,
                        )),
                    })
                    .collect::<Result<_, ArgumentSubstitutionError>>()?,
            ))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let args = args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| evaluate_expanded_module_functions(value, config))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if args.iter().all(|argument| {
                argument.handle().is_none() && argument.walker_value().is_fully_resolved()
            }) {
                let eval_args = args
                    .iter()
                    .map(crate::parser::function_argument_as_eval)
                    .collect::<Vec<_>>();
                crate::builtins::evaluate_builtin_with_config(name.as_str(), &eval_args, config)
                    .map_err(|message| ArgumentSubstitutionError {
                        source_file: None,
                        line: source_line.get().unwrap_or(0),
                        message,
                        parse_error: None,
                    })?
                    .into_value()
                    .map_err(|error| ArgumentSubstitutionError {
                        source_file: None,
                        line: source_line.get().unwrap_or(0),
                        message: error.to_string(),
                        parse_error: None,
                    })?
            } else {
                Value::Deferred(DeferredValue::FunctionCall {
                    name: *name,
                    args,
                    source_line: *source_line,
                })
            }
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => Value::Deferred(DeferredValue::Unavailable {
            name: *name,
            args: args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| evaluate_expanded_module_functions(value, config))
                })
                .collect::<Result<_, _>>()?,
            reason: reason.clone(),
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: _,
            args,
            source_line,
        }) => {
            let args = args
                .iter()
                .map(|argument| {
                    argument
                        .try_map_value(|value| evaluate_expanded_module_functions(value, config))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let line = source_line.get().unwrap_or(function.body.source_line);
            crate::resolver::evaluate_module_resolved_user_function(function, &args, config, line)
                .map_err(|error| function_substitution_error(error, line))?
        }
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                evaluate_expanded_module_functions(access.base(), config)?,
                crate::resource::AccessMissPolicy::Error,
            )
            .map_err(|error| ArgumentSubstitutionError {
                source_file: None,
                line: access.source_line(),
                message: error.to_string(),
                parse_error: None,
            })?,
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(evaluate_expanded_module_functions(inner, config)?),
        )),
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
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::RedactedSecret { hash: _ },
        ) => value.clone(),
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            let _dependency = expanded_unknown_dependency(reason);
            value.clone()
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            let _dependency = expanded_placeholder_dependency(placeholder);
            value.clone()
        }
    })
}

fn function_substitution_error(
    error: crate::parser::ParseError,
    fallback_line: usize,
) -> ArgumentSubstitutionError {
    ArgumentSubstitutionError {
        source_file: error.source_file().map(std::path::Path::to_path_buf),
        line: fallback_line,
        message: error.to_string(),
        parse_error: Some(Box::new(error)),
    }
}

fn expand_module_resource(
    resource: &Resource,
    dependency_values: Option<&IndexMap<String, Value>>,
    module_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<ModuleResolvedResource, ArgumentSubstitutionError> {
    let instance_identity = ResourceIdentity::try_from(resolver.scope.key().to_string())
        .expect("module instance prefix is non-empty");
    expand_module_resource_with_identity(
        resource,
        dependency_values,
        &instance_identity,
        module_call,
        resolver,
    )
}

fn expand_module_resource_with_identity(
    resource: &Resource,
    dependency_values: Option<&IndexMap<String, Value>>,
    instance_identity: &ResourceIdentity,
    module_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<ModuleResolvedResource, ArgumentSubstitutionError> {
    let instance_prefix = resolver.scope.key();
    let mut new_resource = match resource.id.identity_state() {
        ResourceIdentityState::Pending(_) => resource.instantiate(),
        ResourceIdentityState::Resolved(identity) => resource.instantiate_with_identity(
            ResourceIdentity::new(prefix_resource_identity(instance_prefix, identity.as_str())),
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

    let crate::resource::Resource {
        id,
        attributes,
        directives,
        resolved_ignore_changes,
        prefixes,
        binding,
        dependency_bindings,
        module_source: _,
        default_tag_keys,
        loop_provenance,
        quoted_string_attrs,
    } = new_resource;
    let resolved_depends_on = resolve_directive_dependencies(
        &directives.depends_on,
        &directives.depends_on_diagnostic_sources,
        resolver,
    )?;
    let directives = directives.with_resolved_depends_on(resolved_depends_on);

    Ok(crate::resource::Resource {
        id,
        attributes: attributes
            .into_iter()
            .map(|(name, value)| Ok((name, resolve_module_value(&value, resolver)?)))
            .collect::<Result<_, ArgumentSubstitutionError>>()?,
        directives,
        resolved_ignore_changes,
        prefixes,
        binding: binding
            .as_ref()
            .map(|binding| resolver.resolve_declared_binding(binding))
            .transpose()?,
        dependency_bindings: match dependency_values {
            Some(values) => resolve_dependency_values(values.values(), resolver)?,
            None => resolve_dependency_bindings(&dependency_bindings, resolver)?,
        },
        module_source: Some(ModuleSource::expanded_module(
            &resolver.call_path.root().module_name,
            &resolver.call_path.root().instance,
            resolver.scope.clone(),
            resolver.call_path.clone(),
        )),
        default_tag_keys,
        loop_provenance,
        quoted_string_attrs,
    })
}

fn expand_module_data_source(
    data_source: &DataSource,
    dependency_values: Option<&IndexMap<String, Value>>,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<ModuleResolvedDataSource, ArgumentSubstitutionError> {
    let instance_prefix = resolver.scope.key();
    let new_data_source = match data_source.id.identity_state() {
        ResourceIdentityState::Pending(_) => data_source.instantiate(),
        ResourceIdentityState::Resolved(identity) => data_source.instantiate_with_identity(
            ResourceIdentity::new(prefix_resource_identity(instance_prefix, identity.as_str())),
        ),
    };
    let crate::resource::DataSource {
        id,
        attributes,
        directives,
        binding,
        dependency_bindings,
        module_source: _,
        quoted_string_attrs,
    } = new_data_source;
    let resolved_depends_on = resolve_directive_dependencies(
        &directives.depends_on,
        &directives.depends_on_diagnostic_sources,
        resolver,
    )?;
    let directives = directives.with_resolved_depends_on(resolved_depends_on);
    Ok(crate::resource::DataSource {
        id,
        attributes: attributes
            .into_iter()
            .map(|(name, value)| Ok((name, resolve_module_value(&value, resolver)?)))
            .collect::<Result<_, ArgumentSubstitutionError>>()?,
        directives,
        binding: binding
            .as_ref()
            .map(|binding| resolver.resolve_declared_binding(binding))
            .transpose()?,
        dependency_bindings: match dependency_values {
            Some(values) => resolve_dependency_values(values.values(), resolver)?,
            None => resolve_dependency_bindings(&dependency_bindings, resolver)?,
        },
        module_source: Some(ModuleSource::expanded_module(
            &resolver.call_path.root().module_name,
            &resolver.call_path.root().instance,
            resolver.scope.clone(),
            resolver.call_path.clone(),
        )),
        quoted_string_attrs,
    })
}

fn expand_module_composition(
    composition: &crate::resource::AuthoredLiveComposition,
    root_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
    provider_context: crate::validation::typecheck::TypecheckProviderContext<'_>,
) -> Result<ModuleResolvedLiveComposition, ArgumentSubstitutionError> {
    let crate::resource::LiveComposition {
        id,
        signature,
        binding,
        dependency_bindings,
        module_name,
        instance,
        call_path,
        quoted_string_attrs,
    } = composition;
    let crate::resource::LiveCompositionSignature {
        arguments,
        attributes,
        pending_constraints,
    } = signature;

    let arguments = arguments
        .iter()
        .map(|(name, argument)| {
            let (declared_type, origin) = argument.metadata();
            let resolved = match argument.operand_ref() {
                StructuralOperandRef::Value(value) => {
                    let parameter = match value {
                        Value::Deferred(DeferredValue::Placeholder(
                            ParserPlaceholder::ModuleArgument {
                                name,
                                diagnostic: _,
                            },
                        )) => Some(name.as_str()),
                        Value::Deferred(DeferredValue::BindingRef { binding }) => {
                            Some(binding.as_str())
                        }
                        Value::Concrete(
                            ConcreteValue::String(_)
                            | ConcreteValue::EnumIdentifier(_)
                            | ConcreteValue::CanonicalEnum(_)
                            | ConcreteValue::Int(_)
                            | ConcreteValue::Float(_)
                            | ConcreteValue::Bool(_)
                            | ConcreteValue::Duration(_)
                            | ConcreteValue::List(_)
                            | ConcreteValue::StringList(_)
                            | ConcreteValue::Map(_),
                        )
                        | Value::Deferred(
                            DeferredValue::ModuleIf(_)
                            | DeferredValue::ResourceRef { path: _ }
                            | DeferredValue::Coalesce {
                                value: _,
                                default: _,
                            }
                            | DeferredValue::IntegerArithmetic(_)
                            | DeferredValue::Interpolation(_)
                            | DeferredValue::FunctionCall {
                                name: _,
                                args: _,
                                source_line: _,
                            }
                            | DeferredValue::ResolvedFunctionCall {
                                function: _,
                                functions: _,
                                args: _,
                                source_line: _,
                            }
                            | DeferredValue::Unavailable {
                                name: _,
                                args: _,
                                reason: _,
                                source_line: _,
                            }
                            | DeferredValue::Access(_)
                            | DeferredValue::Secret(_)
                            | DeferredValue::RedactedSecret { hash: _ }
                            | DeferredValue::Unknown(
                                ApplyTimeUnknown::UpstreamExport { path: _ }
                                | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                                | ApplyTimeUnknown::PendingReRead(_),
                            )
                            | DeferredValue::Placeholder(
                                ParserPlaceholder::FnParam { name: _ }
                                | ParserPlaceholder::FnLocal { name: _ }
                                | ParserPlaceholder::Loop(LoopSlot::Key)
                                | ParserPlaceholder::Loop(LoopSlot::Index)
                                | ParserPlaceholder::Loop(LoopSlot::Value)
                                | ParserPlaceholder::EmptyInterpolation { source: _ },
                            ),
                        ) => None,
                    };
                    let forwarded_handle = parameter
                        .and_then(|parameter| {
                            resolver
                                .arguments
                                .arguments
                                .get(parameter)
                                .map(|argument| (parameter, argument))
                        })
                        .and_then(|(parameter, argument)| match argument.value_ref() {
                            StructuralOperandRef::Handle {
                                handle,
                                access_base,
                            } => Some((parameter, handle, access_base)),
                            StructuralOperandRef::Value(_)
                            | StructuralOperandRef::Parameter {
                                name: _,
                                access_base: _,
                            } => None,
                        });
                    match forwarded_handle {
                        Some((parameter, handle, access_base)) => {
                            crate::resource::LiveCompositionArgument::from_handle_with_static_type(
                                access_base.clone(),
                                declared_type.clone(),
                                handle.clone().with_alias(parameter),
                                origin,
                            )
                        }
                        None => crate::resource::LiveCompositionArgument::from_value(
                            resolve_module_value(value, resolver)?,
                            declared_type.clone(),
                            origin,
                        ),
                    }
                }
                StructuralOperandRef::Handle {
                    handle,
                    access_base,
                } => crate::resource::LiveCompositionArgument::from_handle_with_static_type(
                    resolver
                        .resolve_value(access_base, ArgumentSubstitutionPosition::AccessBase)?,
                    declared_type.clone(),
                    resolver.resolve_structural_handle(handle)?,
                    origin,
                ),
                StructuralOperandRef::Parameter {
                    name: parameter, ..
                } => {
                    let substituted = resolver
                        .arguments
                        .arguments
                        .get(parameter)
                        .ok_or_else(|| resolver.unknown_binding(parameter))?;
                    match substituted.value_ref() {
                        StructuralOperandRef::Value(value) => {
                            crate::resource::LiveCompositionArgument::from_value(
                                value.clone(),
                                declared_type.clone(),
                                origin,
                            )
                        }
                        StructuralOperandRef::Handle {
                            handle,
                            access_base,
                        } => {
                            crate::resource::LiveCompositionArgument::from_handle_with_static_type(
                                access_base.clone(),
                                declared_type.clone(),
                                handle.clone().with_alias(parameter),
                                origin,
                            )
                        }
                        StructuralOperandRef::Parameter {
                            access_base: forwarded_access_base,
                            ..
                        } => crate::resource::LiveCompositionArgument::from_value(
                            forwarded_access_base.clone(),
                            declared_type.clone(),
                            origin,
                        ),
                    }
                }
            };
            check_composition_argument_type(
                name,
                &resolved,
                provider_context,
                resolver.arguments.default_line,
            )?;
            Ok((name.clone(), resolved))
        })
        .collect::<Result<IndexMap<_, _>, ArgumentSubstitutionError>>()?;

    let attributes = attributes
        .iter()
        .map(|(name, attribute)| {
            Ok((
                name.clone(),
                crate::resource::LiveCompositionAttribute::from_value(
                    resolve_module_value(&attribute.to_value(), resolver)?,
                    attribute.declared_type().cloned(),
                )
                .with_source_argument(attribute.source_argument().map(str::to_string)),
            ))
        })
        .collect::<Result<IndexMap<_, _>, ArgumentSubstitutionError>>()?;

    let pending_constraints = pending_constraints
        .iter()
        .map(|constraint| {
            Ok(crate::resource::PendingModuleConstraint {
                id: constraint.id.clone(),
                expression: resolve_module_constraint_expression(&constraint.expression, resolver)?,
                message: constraint.message.clone(),
            })
        })
        .collect::<Result<Vec<_>, ArgumentSubstitutionError>>()?;

    for call in call_path.calls() {
        if let Some(binding) = &call.binding {
            resolver.resolve_declared_binding(binding)?;
        }
    }
    let mut call_path = call_path
        .clone()
        .resolve_bindings(&mut |binding| resolver.scope.resolve(binding));
    for call in call_path.calls_mut() {
        call.instance = prefix_resource_identity(resolver.scope.key(), &call.instance);
    }
    call_path = call_path.prepend(root_call.clone());

    Ok(crate::resource::LiveComposition {
        id: id.instantiate_with_identity(ResourceIdentity::new(prefix_resource_identity(
            resolver.scope.key(),
            id.identity_str(),
        ))),
        signature: crate::resource::LiveCompositionSignature {
            arguments,
            attributes,
            pending_constraints,
        },
        binding: binding
            .as_ref()
            .map(|binding| resolver.resolve_declared_binding(binding))
            .transpose()?,
        dependency_bindings: resolve_dependency_bindings(dependency_bindings, resolver)?,
        module_name: module_name.clone(),
        instance: prefix_resource_identity(resolver.scope.key(), instance),
        call_path,
        quoted_string_attrs: quoted_string_attrs.clone(),
    })
}

/// Re-evaluate a nested composition after its outer caller has supplied the
/// arguments that were unavailable during the nested module's first
/// expansion.
fn settle_prefixed_composition_constraints(
    composition: &mut ModuleResolvedLiveComposition,
    record_outcomes: bool,
) -> Result<Option<ModuleCallConstraintReport>, ModuleError> {
    if composition.signature.pending_constraints.is_empty() {
        return Ok(None);
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
    let mut outcomes = Vec::new();
    for evaluated in evaluated {
        let id = evaluated.constraint().id().clone();
        match evaluated.evaluation() {
            ConstraintEvaluation::Satisfied => outcomes.push(ResolvedModuleConstraintOutcome {
                id,
                status: ResolvedModuleConstraintStatus::Satisfied,
            }),
            ConstraintEvaluation::Pending => {
                outcomes.push(ResolvedModuleConstraintOutcome {
                    id,
                    status: ResolvedModuleConstraintStatus::Pending,
                });
                pending.push(evaluated.into_constraint());
            }
            ConstraintEvaluation::Violated(_) | ConstraintEvaluation::EvalError(_) => {
                let binding = composition.binding.as_ref().map(BindingAddress::as_str);
                if record_outcomes {
                    let diagnostic = evaluated
                        .module_diagnostic(
                            &composition.module_name,
                            &composition.instance,
                            binding,
                            Some(&composition.call_path.immediate().id),
                        )
                        .expect("concrete nested constraint outcome has a diagnostic");
                    outcomes.push(ResolvedModuleConstraintOutcome {
                        id,
                        status: ResolvedModuleConstraintStatus::Failed {
                            diagnostic: Box::new(diagnostic),
                            origin: ResolvedModuleConstraintOrigin::Call,
                        },
                    });
                } else {
                    return Err(evaluated
                        .module_error(
                            &composition.module_name,
                            &composition.instance,
                            binding,
                            Some(&composition.call_path.immediate().id),
                        )
                        .expect("concrete nested constraint outcome has a diagnostic"));
                }
            }
        }
    }
    composition.signature.pending_constraints = pending;
    if record_outcomes {
        Ok(Some(ModuleCallConstraintReport::new(
            crate::parser::ModuleCallDiagnosticSource {
                id: composition.call_path.immediate().id.clone(),
            },
            composition.instance.clone(),
            outcomes,
        )))
    } else {
        Ok(None)
    }
}

fn check_composition_argument_type<P: crate::resource::PlaceholderKind, B: BindingAddress>(
    parameter: &str,
    argument: &crate::resource::LiveCompositionArgument<P, B>,
    config: crate::validation::typecheck::TypecheckProviderContext<'_>,
    line: usize,
) -> Result<(), ArgumentSubstitutionError> {
    let Some(declared_type) = argument.declared_type().known() else {
        return Ok(());
    };
    let Some(resolved_parameter) = crate::validation::resolve_parameter_type_for_check(
        declared_type,
        config.as_provider_context(),
    )
    .map_err(|error| ArgumentSubstitutionError {
        source_file: None,
        line,
        message: error.to_string(),
        parse_error: None,
    })?
    else {
        return Ok(());
    };

    if let StructuralOperandRef::Handle { handle, .. } = argument.operand_ref() {
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

    config
        .check(
            &resolved_parameter.as_type_expr(),
            argument.value(),
            crate::validation::typecheck::TypecheckInference::Unavailable,
        )
        .map_err(|mismatch| ArgumentSubstitutionError {
            source_file: None,
            line,
            message: format!("parameter '{parameter}': {mismatch}"),
            parse_error: None,
        })
}

fn resolve_module_constraint_expression(
    expression: &crate::parser::ValidateExpr<ParserPlaceholder, RelativeBindingName>,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<
    crate::parser::ValidateExpr<ParserPlaceholder, AbsoluteBindingAddress>,
    ArgumentSubstitutionError,
> {
    use crate::parser::ValidateExpr;

    Ok(match expression {
        ValidateExpr::Var(argument) => ValidateExpr::Var(argument.clone()),
        ValidateExpr::Access { argument, path } => ValidateExpr::Access {
            argument: argument.clone(),
            path: path.clone(),
        },
        ValidateExpr::BoundArgument {
            argument,
            operand,
            value,
        } => ValidateExpr::BoundArgument {
            argument: argument.clone(),
            operand: operand.clone(),
            value: resolve_module_value(value, resolver)?,
        },
        ValidateExpr::Bool(value) => ValidateExpr::Bool(*value),
        ValidateExpr::Int(value) => ValidateExpr::Int(*value),
        ValidateExpr::Float(value) => ValidateExpr::Float(*value),
        ValidateExpr::Duration(value) => ValidateExpr::Duration(*value),
        ValidateExpr::String(value) => ValidateExpr::String(value.clone()),
        ValidateExpr::Null => ValidateExpr::Null,
        ValidateExpr::IntegerArithmetic {
            lhs,
            op,
            rhs,
            operator_span,
        } => ValidateExpr::IntegerArithmetic {
            lhs: Box::new(resolve_module_constraint_expression(lhs, resolver)?),
            op: *op,
            rhs: Box::new(resolve_module_constraint_expression(rhs, resolver)?),
            operator_span: *operator_span,
        },
        ValidateExpr::Compare { lhs, op, rhs } => ValidateExpr::Compare {
            lhs: Box::new(resolve_module_constraint_expression(lhs, resolver)?),
            op: op.clone(),
            rhs: Box::new(resolve_module_constraint_expression(rhs, resolver)?),
        },
        ValidateExpr::And(lhs, rhs) => ValidateExpr::And(
            Box::new(resolve_module_constraint_expression(lhs, resolver)?),
            Box::new(resolve_module_constraint_expression(rhs, resolver)?),
        ),
        ValidateExpr::Or(lhs, rhs) => ValidateExpr::Or(
            Box::new(resolve_module_constraint_expression(lhs, resolver)?),
            Box::new(resolve_module_constraint_expression(rhs, resolver)?),
        ),
        ValidateExpr::Not(inner) => ValidateExpr::Not(Box::new(
            resolve_module_constraint_expression(inner, resolver)?,
        )),
        ValidateExpr::FunctionCall { name, args } => ValidateExpr::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|argument| resolve_module_constraint_expression(argument, resolver))
                .collect::<Result<_, _>>()?,
        },
    })
}

fn expand_parsed_deferred_for_expression(
    deferred: &ParsedDeferredForExpression<ParserPlaceholder, RelativeBindingName>,
    dependency_values: Option<&IndexMap<String, Value>>,
    module_call: &crate::resource::ExpandedCompositionCall,
    resolver: &ModuleScopeResolver<'_>,
) -> Result<
    ParsedDeferredForExpression<ParserPlaceholder, AbsoluteBindingAddress>,
    ArgumentSubstitutionError,
> {
    let ParsedDeferredForExpression {
        file,
        line,
        span,
        header,
        target,
        iterable,
        iterable_state,
        binding,
        body,
    } = deferred;
    let instance_identity = ResourceIdentity::try_from(resolver.scope.key().to_string())
        .expect("module instance prefix is non-empty");
    let legacy_parent = module_call
        .loop_origin()
        .map(|origin| origin.legacy_address().clone())
        .unwrap_or_else(|| LegacyLoopAddress::new(instance_identity.as_str()));
    let target = match target {
        crate::parser::DeferredForTarget::Bound { binding } => {
            crate::parser::DeferredForTarget::Bound {
                binding: resolver.resolve_declared_binding(binding)?,
            }
        }
        crate::parser::DeferredForTarget::Unbound { template } => {
            let mut template = template.clone();
            template.prefix_for_module(&instance_identity, &legacy_parent);
            crate::parser::DeferredForTarget::Unbound { template }
        }
    };
    let iterable_state = if matches!(iterable_state, ParsedDeferredForIterableState::Deferred)
        && crate::parser::contains_module_argument_matching(iterable, &mut |name| {
            resolver
                .arguments
                .arguments
                .get(name)
                .is_some_and(|argument| argument.forwarded_from_enclosing_parameter)
        }) {
        ParsedDeferredForIterableState::DependsOnEnclosingModuleArgument
    } else {
        iterable_state.clone()
    };

    // A bare, unbound iterable is retained specifically so the finalization
    // seam can emit the parser's source-owned UndefinedIdentifier diagnostic
    // and collect every such error in the module. It must still cross this
    // phase as an absolute value, but checking membership here would replace
    // that diagnostic with the expander's first-error-only "unknown binding".
    let iterable = if matches!(
        &iterable_state,
        ParsedDeferredForIterableState::UndefinedIdentifier { name: _ }
    ) {
        iterable.clone().resolve_module(resolver.scope)
    } else {
        resolve_module_value(iterable, resolver)?
    };
    let body = match body {
        DeferredForBody::Resource(resource) => {
            DeferredForBody::Resource(Box::new(expand_module_resource_with_identity(
                resource,
                dependency_values,
                &instance_identity,
                module_call,
                resolver,
            )?))
        }
        DeferredForBody::Read(data_source) => DeferredForBody::Read(Box::new(
            expand_module_data_source(data_source, dependency_values, resolver)?,
        )),
        DeferredForBody::ModuleCall(call) => {
            DeferredForBody::ModuleCall(resolve_nested_module_call(call, resolver)?)
        }
    };

    Ok(ParsedDeferredForExpression {
        file: file.clone(),
        line: *line,
        span: *span,
        header: header.clone(),
        target,
        iterable,
        iterable_state,
        binding: binding.clone(),
        body,
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
                    caller_reference: None,
                    forwarded_from_enclosing_parameter: false,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    let substituted = substitute_arguments_with_position(
        value,
        ArgumentSubstitutions {
            arguments: &arguments,
            default_line: 1,
            caller_source_file: std::path::Path::new("test.crn"),
            binding_index: None,
            config: &config,
        },
        ArgumentSubstitutionPosition::Value,
    )?;
    select_authored_module_ifs_for_test(&substituted, &config)
}

#[cfg(test)]
fn select_authored_module_ifs_for_test(
    value: &Value,
    config: &crate::parser::ProviderContext,
) -> Result<Value, ArgumentSubstitutionError> {
    use crate::resource::{ConcreteValue, DeferredValue, Value};

    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            match crate::parser::decide_if_condition(deferred.condition().value(), config).map_err(
                |error| ArgumentSubstitutionError {
                    source_file: Some(deferred.condition().source_file().to_path_buf()),
                    line: deferred.condition().source_line(),
                    message: error.to_string(),
                    parse_error: None,
                },
            )? {
                Some(true) => select_authored_module_ifs_for_test(deferred.then_value(), config)?,
                Some(false) => select_authored_module_ifs_for_test(deferred.else_value(), config)?,
                None => value.clone(),
            }
        }
        Value::Concrete(ConcreteValue::List(values)) => Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .map(|value| select_authored_module_ifs_for_test(value, config))
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(values)) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        select_authored_module_ifs_for_test(value, config)?,
                    ))
                })
                .collect::<Result<_, ArgumentSubstitutionError>>()?,
        )),
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(select_authored_module_ifs_for_test(inner, config)?),
        )),
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
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::Coalesce {
                value: _,
                default: _,
            }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall {
                name: _,
                args: _,
                source_line: _,
            }
            | DeferredValue::Unavailable {
                name: _,
                args: _,
                reason: _,
                source_line: _,
            }
            | DeferredValue::ResolvedFunctionCall {
                function: _,
                functions: _,
                args: _,
                source_line: _,
            }
            | DeferredValue::Access(_)
            | DeferredValue::RedactedSecret { hash: _ },
        ) => value.clone(),
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            let _dependency = expanded_unknown_dependency(reason);
            value.clone()
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            let _dependency = expanded_placeholder_dependency(placeholder);
            value.clone()
        }
    })
}

fn substitute_function_argument<B: BindingAddress>(
    argument: &crate::resource::FunctionArgument<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
) -> Result<crate::resource::FunctionArgument<ParserPlaceholder, B>, ArgumentSubstitutionError> {
    if let Some(name) = argument.pending_parameter()
        && let Some(substitution) = arguments.arguments.get(name)
    {
        return match substitution.value_ref() {
            StructuralOperandRef::Handle {
                handle,
                access_base: _,
            } => Ok(crate::resource::FunctionArgument::from_handle(
                handle.clone(),
            )),
            StructuralOperandRef::Value(value) => {
                Ok(crate::resource::FunctionArgument::from_value(value.clone()))
            }
            StructuralOperandRef::Parameter {
                name: _,
                access_base: _,
            } => substitute_arguments_with_position(
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

fn substitute_module_if_paths_in_secret<B: BindingAddress>(
    value: &crate::resource::Value<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
    position: ArgumentSubstitutionPosition,
) -> Result<crate::resource::Value<ParserPlaceholder, B>, ArgumentSubstitutionError> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    if !super::deferred_if::value_contains_module_if(value) {
        return Ok(value.clone());
    }
    if matches!(value, Value::Deferred(DeferredValue::ModuleIf(_))) {
        return substitute_arguments_with_position(value, arguments, position);
    }

    Ok(match value {
        Value::Concrete(ConcreteValue::List(values)) => Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .map(|value| substitute_module_if_paths_in_secret(value, arguments, position))
                .collect::<Result<_, _>>()?,
        )),
        Value::Concrete(ConcreteValue::Map(values)) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(name, value)| {
                    Ok((
                        name.clone(),
                        substitute_module_if_paths_in_secret(value, arguments, position)?,
                    ))
                })
                .collect::<Result<_, ArgumentSubstitutionError>>()?,
        )),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            Value::Deferred(DeferredValue::Interpolation(
                parts
                    .iter()
                    .map(|part| match part {
                        InterpolationPart::Literal(literal) => {
                            Ok(InterpolationPart::Literal(literal.clone()))
                        }
                        InterpolationPart::Expr(value) => Ok(InterpolationPart::Expr(
                            substitute_module_if_paths_in_secret(value, arguments, position)?,
                        )),
                    })
                    .collect::<Result<_, ArgumentSubstitutionError>>()?,
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
                    argument.try_map_value(|value| {
                        substitute_module_if_paths_in_secret(value, arguments, position)
                    })
                })
                .collect::<Result<_, _>>()?,
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => Value::Deferred(DeferredValue::Unavailable {
            name: *name,
            args: args
                .iter()
                .map(|argument| {
                    argument.try_map_value(|value| {
                        substitute_module_if_paths_in_secret(value, arguments, position)
                    })
                })
                .collect::<Result<_, _>>()?,
            reason: reason.clone(),
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
                    argument.try_map_value(|value| {
                        substitute_module_if_paths_in_secret(value, arguments, position)
                    })
                })
                .collect::<Result<_, _>>()?,
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                substitute_module_if_paths_in_secret(access.base(), arguments, position)?,
                AccessMissPolicy::Error,
            )
            .map_err(|error| ArgumentSubstitutionError {
                source_file: None,
                line: access.source_line(),
                message: error.to_string(),
                parse_error: None,
            })?,
        Value::Deferred(DeferredValue::Secret(inner)) => {
            Value::Deferred(DeferredValue::Secret(Box::new(
                substitute_module_if_paths_in_secret(inner, arguments, position)?,
            )))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(substitute_module_if_paths_in_secret(
                    value, arguments, position,
                )?),
                default: Box::new(substitute_module_if_paths_in_secret(
                    default, arguments, position,
                )?),
            })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => expression
            .evaluate_with_operands(
                substitute_module_if_paths_in_secret(expression.lhs(), arguments, position)?,
                substitute_module_if_paths_in_secret(expression.rhs(), arguments, position)?,
            )
            .map_err(|error| ArgumentSubstitutionError {
                source_file: None,
                line: expression
                    .operator_span()
                    .line()
                    .unwrap_or(arguments.default_line),
                message: error.to_string(),
                parse_error: None,
            })?,
        Value::Deferred(DeferredValue::ModuleIf(_)) => {
            unreachable!("module-if values return before container traversal")
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
            DeferredValue::ResourceRef { path: _ }
            | DeferredValue::BindingRef { binding: _ }
            | DeferredValue::RedactedSecret { hash: _ },
        ) => value.clone(),
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            let _dependency = expanded_unknown_dependency(reason);
            value.clone()
        }
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            let _dependency = expanded_placeholder_dependency(placeholder);
            value.clone()
        }
    })
}

fn substitute_arguments_with_position<B: BindingAddress>(
    value: &crate::resource::Value<ParserPlaceholder, B>,
    arguments: ArgumentSubstitutions<'_, B>,
    position: ArgumentSubstitutionPosition,
) -> Result<crate::resource::Value<ParserPlaceholder, B>, ArgumentSubstitutionError> {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};

    let substituted = match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            let condition = substitute_arguments_with_position(
                deferred.condition().value(),
                arguments,
                ArgumentSubstitutionPosition::Value,
            )
            .map_err(|error| error.with_source_file(deferred.condition().source_file()))?;
            Value::Deferred(DeferredValue::ModuleIf(Box::new(DeferredValueIf::new(
                DeferredIfCondition::new_with_source_expression(
                    condition,
                    deferred.condition().source_file().to_path_buf(),
                    deferred.condition().source_line(),
                    deferred.condition().source_expression().to_string(),
                ),
                substitute_arguments_with_position(deferred.then_value(), arguments, position)?,
                substitute_arguments_with_position(deferred.else_value(), arguments, position)?,
            ))))
        }
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument {
            name,
            diagnostic,
        })) => {
            let Some(argument) = arguments.arguments.get(name) else {
                return Ok(value.clone());
            };
            let line = diagnostic
                .source_line()
                .unwrap_or_else(|| argument.source_line(arguments.default_line));
            let source_file = diagnostic
                .authored_location()
                .map(|(source_file, _)| source_file.to_path_buf());
            match argument.value_ref() {
                StructuralOperandRef::Value(value) => value.clone(),
                StructuralOperandRef::Handle { .. }
                    if matches!(position, ArgumentSubstitutionPosition::Value) =>
                {
                    return Err(ArgumentSubstitutionError {
                        source_file,
                        line,
                        message: BareStructuralHandle {
                            binding: diagnostic.diagnostic_name().unwrap_or(name).to_string(),
                            kind: StructuralHandleKind::ModuleParameter,
                        }
                        .to_string(),
                        parse_error: None,
                    });
                }
                StructuralOperandRef::Handle { access_base, .. } => access_base.clone(),
                StructuralOperandRef::Parameter {
                    name: _,
                    access_base,
                } => {
                    let mut forwarded_value = access_base.clone();
                    if let Value::Deferred(DeferredValue::Placeholder(
                        ParserPlaceholder::ModuleArgument {
                            diagnostic: forwarded_diagnostic,
                            ..
                        },
                    )) = &mut forwarded_value
                    {
                        forwarded_diagnostic.forward_from(diagnostic, name);
                    }
                    forwarded_value
                }
            }
        }
        Value::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::FnParam { name: _ }
            | ParserPlaceholder::FnLocal { name: _ }
            | ParserPlaceholder::Loop(LoopSlot::Key)
            | ParserPlaceholder::Loop(LoopSlot::Index)
            | ParserPlaceholder::Loop(LoopSlot::Value)
            | ParserPlaceholder::EmptyInterpolation { source: _ },
        )) => value.clone(),
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            let Some(argument) = arguments.arguments.get(binding.as_str()) else {
                return Ok(value.clone());
            };
            let source_line = argument.source_line(arguments.default_line);
            argument
                .substitution(binding.as_str(), ArgumentSubstitutionPosition::Value)
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
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = substitute_arguments_with_position(
                expression.lhs(),
                arguments,
                ArgumentSubstitutionPosition::Value,
            )?;
            let rhs = substitute_arguments_with_position(
                expression.rhs(),
                arguments,
                ArgumentSubstitutionPosition::Value,
            )?;
            expression
                .evaluate_with_operands(lhs, rhs)
                .map_err(|error| ArgumentSubstitutionError {
                    source_file: None,
                    line: expression
                        .operator_span()
                        .line()
                        .unwrap_or(arguments.default_line),
                    message: error.to_string(),
                    parse_error: None,
                })?
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
            let substituted_parts: Vec<_> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => substitute_arguments_with_position(
                        v,
                        arguments,
                        ArgumentSubstitutionPosition::Value,
                    )
                    .map(InterpolationPart::Expr),
                    InterpolationPart::Literal(literal) => {
                        Ok(InterpolationPart::Literal(literal.clone()))
                    }
                })
                .collect::<Result<_, _>>()?;
            Value::Deferred(DeferredValue::Interpolation(substituted_parts))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => Value::Deferred(DeferredValue::FunctionCall {
            name: *name,
            args: args
                .iter()
                .map(|argument| substitute_function_argument(argument, arguments))
                .collect::<Result<_, _>>()?,
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => Value::Deferred(DeferredValue::Unavailable {
            name: *name,
            args: args
                .iter()
                .map(|argument| substitute_function_argument(argument, arguments))
                .collect::<Result<_, _>>()?,
            reason: reason.clone(),
            source_line: *source_line,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let args = args
                .iter()
                .map(|argument| substitute_function_argument(argument, arguments))
                .collect::<Result<Vec<_>, _>>()?;
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: function.clone(),
                functions: functions.clone(),
                args,
                source_line: *source_line,
            })
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
        | Value::Deferred(DeferredValue::RedactedSecret { hash: _ }) => value.clone(),
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            let _dependency = expanded_unknown_dependency(reason);
            value.clone()
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(substitute_module_if_paths_in_secret(
                inner,
                arguments,
                ArgumentSubstitutionPosition::Value,
            )?),
        )),
    };

    if let Some(binding_index) = arguments.binding_index
        && let Some((argument, postfix)) = module_parameter_postfix(value, arguments.arguments)
        && let Some(reference) = arguments
            .arguments
            .get(&argument)
            .and_then(|resolved| resolved.caller_reference.clone())
        && let RefType::UnknownAttribute(
            error @ RefTypeError::InvalidAccess {
                path: _,
                step: _,
                actual: _,
            },
        ) = binding_index.postfix_ref_type(append_reference_path(reference, &postfix).as_ref())
    {
        let (source_file, line) = match value {
            Value::Deferred(DeferredValue::Access(access)) => (None, access.source_line()),
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::ModuleArgument {
                name: _,
                diagnostic,
            })) => (
                diagnostic
                    .authored_location()
                    .map(|(source_file, _)| source_file.to_path_buf()),
                diagnostic.source_line().unwrap_or(arguments.default_line),
            ),
            Value::Concrete(
                ConcreteValue::String(_)
                | ConcreteValue::EnumIdentifier(_)
                | ConcreteValue::CanonicalEnum(_)
                | ConcreteValue::Int(_)
                | ConcreteValue::Float(_)
                | ConcreteValue::Bool(_)
                | ConcreteValue::Duration(_)
                | ConcreteValue::List(_)
                | ConcreteValue::StringList(_)
                | ConcreteValue::Map(_),
            )
            | Value::Deferred(
                DeferredValue::ModuleIf(_)
                | DeferredValue::ResourceRef { path: _ }
                | DeferredValue::BindingRef { binding: _ }
                | DeferredValue::Coalesce {
                    value: _,
                    default: _,
                }
                | DeferredValue::IntegerArithmetic(_)
                | DeferredValue::Interpolation(_)
                | DeferredValue::FunctionCall {
                    name: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::Unavailable {
                    name: _,
                    args: _,
                    reason: _,
                    source_line: _,
                }
                | DeferredValue::ResolvedFunctionCall {
                    function: _,
                    functions: _,
                    args: _,
                    source_line: _,
                }
                | DeferredValue::Secret(_)
                | DeferredValue::RedactedSecret { hash: _ }
                | DeferredValue::Unknown(
                    ApplyTimeUnknown::UpstreamExport { path: _ }
                    | ApplyTimeUnknown::UpstreamAllExports { binding: _ }
                    | ApplyTimeUnknown::PendingReRead(_),
                )
                | DeferredValue::Placeholder(
                    ParserPlaceholder::FnParam { name: _ }
                    | ParserPlaceholder::FnLocal { name: _ }
                    | ParserPlaceholder::Loop(LoopSlot::Key)
                    | ParserPlaceholder::Loop(LoopSlot::Index)
                    | ParserPlaceholder::Loop(LoopSlot::Value)
                    | ParserPlaceholder::EmptyInterpolation { source: _ },
                ),
            ) => (None, arguments.default_line),
        };
        return Err(ArgumentSubstitutionError {
            source_file,
            line,
            message: error.to_string(),
            parse_error: None,
        });
    }

    Ok(substituted)
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
        return name.to_string();
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
/// qualifies, rewrite every identity-bearing resource/composition and then
/// traverse the complete finalized file to move every absolute binding slot
/// to the same state-backed address.
///
/// `find_state_names_by_type` returns every state resource name for a given
/// `(provider, resource_type)` — the reconciler uses them to discover which
/// instance prefixes already exist in state.
pub fn reconcile_anonymous_module_instances<E, P, T>(
    file: &mut crate::parser::File<
        E,
        P,
        T,
        crate::parser::ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>,
        AbsoluteBindingAddress,
    >,
    find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
    claims: &crate::identifier::StateBlockClaims,
) where
    P: PlaceholderKind,
    T: PlaceholderKind,
    E: crate::parser::ExportParamLike<P, AbsoluteBindingAddress>,
{
    use std::collections::{HashMap, HashSet};

    // Collect current (provider, resource_type) pairs that appear in the
    // expanded DSL — we'll query state for matching entries.
    let mut touched_types: HashSet<(String, String)> = HashSet::new();
    for r in &file.resources {
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
    for r in &file.resources {
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

    // Apply remaps to string identities first. Typed binding addresses move
    // together in one exhaustive traversal below.
    for r in &mut file.resources {
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
            if let Some(crate::resource::ModuleSource::Module(module)) = &r.module_source {
                let mut remapped_call_path = module.module_call_path().clone();
                for call in remapped_call_path.calls_mut() {
                    if let Some(rewritten) = rewrite_name_prefix(&call.instance, &prefix_remap) {
                        call.instance = rewritten;
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

    for data_source in &mut file.data_sources {
        let Some(identity) = data_source.id.identity_str() else {
            continue;
        };
        let Some((prefix, rest)) = split_instance_prefix(identity) else {
            continue;
        };
        let Some((module_name, simhash)) = parse_synthetic_instance_prefix(prefix) else {
            continue;
        };
        if let Some(&target) = prefix_remap.get(&(module_name.to_string(), simhash)) {
            let old_prefix = prefix.to_string();
            let new_prefix = format!("{}_{:016x}", module_name, target);
            data_source
                .id
                .set_identity(ResourceIdentity::new(format!("{}.{}", new_prefix, rest)));
            if let Some(crate::resource::ModuleSource::Module(module)) = &data_source.module_source
            {
                let mut remapped_call_path = module.module_call_path().clone();
                for call in remapped_call_path.calls_mut() {
                    if let Some(rewritten) = rewrite_name_prefix(&call.instance, &prefix_remap) {
                        call.instance = rewritten;
                    }
                }
                data_source.module_source = Some(crate::resource::ModuleSource::expanded_module(
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

    for composition in &mut file.compositions {
        if let Some(identity) = rewrite_name_prefix(composition.id.identity_str(), &prefix_remap) {
            composition.id = composition
                .id
                .instantiate_with_identity(ResourceIdentity::new(identity));
        }
        if let Some(rewritten) = rewrite_name_prefix(&composition.instance, &prefix_remap) {
            composition.instance = rewritten;
        }
        for call in composition.call_path.calls_mut() {
            if let Some(rewritten) = rewrite_name_prefix(&call.instance, &prefix_remap) {
                call.instance = rewritten;
            }
        }
    }

    let mut remap_binding = |binding: &AbsoluteBindingAddress| {
        rewrite_binding_prefix(binding, &prefix_remap).unwrap_or_else(|| binding.clone())
    };
    file.map_absolute_bindings(&mut remap_binding);
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

fn rewrite_binding_prefix(
    binding: &crate::resource::AbsoluteBindingAddress,
    remap: &std::collections::HashMap<(String, SimHash), SimHash>,
) -> Option<crate::resource::AbsoluteBindingAddress> {
    let (prefix, _) = split_instance_prefix(binding.as_str())?;
    let (module, simhash) = parse_synthetic_instance_prefix(prefix)?;
    let target = remap.get(&(module.to_string(), simhash))?;
    let new_prefix = format!("{}_{:016x}", module, target);
    binding.remap_instance_prefix(prefix, &new_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut resource: crate::resource::Resource<
            ModuleSource,
            ParserPlaceholder,
            AbsoluteBindingAddress,
        > = crate::resource::Resource::with_provider(
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
        let mut file = crate::parser::ModuleResolvedParsedFile {
            resources: vec![resource],
            ..Default::default()
        };

        reconcile_anonymous_module_instances(
            &mut file,
            &|_, _| vec![format!("{state_prefix}.leaf")],
            &crate::identifier::StateBlockClaims::empty(),
        );

        let expected_name = format!("{state_prefix}.leaf");
        assert_eq!(
            file.resources[0].id.identity_str(),
            Some(expected_name.as_str())
        );
        let [module_prefix] = file.resources[0].loop_provenance.module_prefixes() else {
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

        let mut resource: crate::resource::Resource<
            ModuleSource,
            ParserPlaceholder,
            AbsoluteBindingAddress,
        > = crate::resource::Resource::with_provider(
            "mock",
            "test.Leaf",
            ResourceIdentity::new(format!("{current_prefix}.leaf")),
            None,
        );
        resource.loop_provenance = crate::resource::LoopProvenance::direct(direct);
        let mut file = crate::parser::ModuleResolvedParsedFile {
            resources: vec![resource],
            ..Default::default()
        };

        reconcile_anonymous_module_instances(
            &mut file,
            &|_, _| vec![format!("{state_prefix}.leaf")],
            &crate::identifier::StateBlockClaims::empty(),
        );

        let expected_name = format!("{state_prefix}.leaf");
        assert_eq!(
            file.resources[0].id.identity_str(),
            Some(expected_name.as_str())
        );
        assert_eq!(
            file.resources[0]
                .loop_provenance
                .direct_origin()
                .expect("direct origin retained")
                .legacy_address()
                .exact_state_name(),
            format!("{state_prefix}._for0[0]"),
            "legacy migration candidates derived from an anonymous module prefix must move with reconciliation",
        );
    }
}
