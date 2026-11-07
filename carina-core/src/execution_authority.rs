//! Permits come from scheduler releases of plans core builds from state rows,
//! parsed/validated configuration or saved-plan values. The CLI authenticates
//! those inputs and checks lineage, serial and fingerprints. Core takes write
//! targets and ordering inputs from those source values.
//!
//! A hand-built delete, including a deposed generation, cannot enter destroy:
//!
//! ```compile_fail
//! use carina_core::{effect::{Effect, EffectGeneration}, executor::{DeleteScheduler, DeleteRelease},
//!     plan::Plan, provider::{AppliedIdentifier, Provider},
//!     resource::{ResourceId, ResolvedResourceId, DeposedKey}, schema::SchemaRegistry};
//! async fn forged_delete(provider: &dyn Provider, schemas: &SchemaRegistry) {
//!     let mut plan = Plan::new();
//!     plan.add(Effect::Delete {
//!         id: ResolvedResourceId::try_new(ResourceId::with_provider_identity("p", "Bucket", "chosen", None)).unwrap(),
//!         identifier: AppliedIdentifier::new("chosen-physical-id").unwrap(),
//!         generation: EffectGeneration::Deposed(DeposedKey::new_unique()),
//!         directives: Default::default(), binding: None, dependencies: Default::default(),
//!         explicit_dependencies: Default::default(), blocked_by_updates: Default::default(),
//!     });
//!     let plan = plan.admit(schemas).unwrap();
//!     let mut scheduler = DeleteScheduler::for_destroy(&plan, &[], &Default::default()).unwrap();
//!     if let Some(DeleteRelease::Ready(delete)) = scheduler.release(0) {
//!         delete.dispatch(provider, schemas).await;
//!     }
//! }
//! ```
//!
//! Bootstrap creation must use a target selected from the supplied configuration.
//! This pins the absence of a public constructor taking an independent resource;
//! it does not establish the authenticity of the configuration value:
//!
//! ```compile_fail
//! use carina_core::{effect::Effect, executor::{prepare_initial_create, ProviderPreparationContext},
//!     plan::Plan, parser::ExpandedInferredFile, resource::Resource,
//!     resolved_graph::resolve_expanded_graph, input_rules::InputTypeContext, schema::SchemaRegistry};
//! async fn forged_create(context: &ProviderPreparationContext<'_>, schemas: &SchemaRegistry) {
//!     let resource = Resource::with_provider("p", "Bucket", "chosen", None);
//!     let mut plan = Plan::new();
//!     let graph = resolve_expanded_graph(ExpandedInferredFile::default().into()).unwrap();
//!     plan.add(Effect::Create(graph.resolve_resource(resource, schemas).unwrap()));
//!     let Effect::Create(target) = &plan.effects()[0] else { unreachable!() };
//!     let checked = graph.initial_create_plan(Resource::from_id(target.id.clone()), schemas)
//!         .unwrap().check_inputs_with_types(schemas, &InputTypeContext::empty()).unwrap();
//!     let _ = prepare_initial_create(&checked, context).await;
//! }
//! ```
//!
//! The public backend constructor derives its target from the supplied configuration.
//! These doctests pin private raw-resource entry points, not source authenticity:
//!
//! ```compile_fail
//! use carina_core::{executor::{prepare_initial_delete, dispatch_delete}, parser::ExpandedInferredFile,
//!     provider::{AppliedIdentifier, Provider}, resource::Resource,
//!     resolved_graph::resolve_expanded_graph, schema::SchemaRegistry};
//! async fn forged_bucket(provider: &dyn Provider, schemas: &SchemaRegistry) {
//!     let resource = Resource::with_provider("p", "Bucket", "chosen", None);
//!     let graph = resolve_expanded_graph(ExpandedInferredFile::default().into()).unwrap();
//!     let plan = graph.initial_delete_plan(resource, AppliedIdentifier::new("chosen-id").unwrap(), schemas)
//!         .unwrap().admit(schemas).unwrap();
//!     let permit = prepare_initial_delete(&plan).unwrap();
//!     let _ = dispatch_delete(provider, permit, schemas).await;
//! }
//! ```
//!
//! Ordinary replacement admission cannot authorize an executor run:
//!
//! ```compile_fail
//! use carina_core::{plan::Plan, schema::SchemaRegistry};
//! let schemas = SchemaRegistry::new();
//! let checked = Plan::new().admit(&schemas).unwrap()
//!     .check_inputs(&schemas, &std::collections::HashMap::new()).unwrap();
//! ```
//!
//! Saved execution cannot be minted by relabeling a raw plan:
//!
//! ```compile_fail
//! use carina_core::{plan::Plan, input_rules::FinalizedPlan, resource::SavedModuleSource};
//! let plan: Plan<SavedModuleSource> = Plan::new().into();
//! let finalized = FinalizedPlan::from_saved_plan(plan);
//! ```
//!
//! Ancillary finalization cannot replace the differ's plan with arbitrary writes:
//!
//! ```compile_fail
//! use carina_core::{differ::PendingCascadePlan, plan::Plan};
//! fn replace(builder: PendingCascadePlan, forged: Plan) {
//!     let _ = builder.finish(std::iter::empty(), |plan| *plan = forged);
//! }
//! ```
//!
//! The current scheduler signature also rejects ordinary replacement admission:
//!
//! ```compile_fail,E0308
//! use carina_core::{executor::DeleteScheduler, plan::AdmittedPlan};
//! fn forged(plan: &AdmittedPlan) { let _ = DeleteScheduler::for_destroy(plan); }
//! ```
//!
//! Finalization cannot append a caller's effect through mutable plan access:
//!
//! ```compile_fail,E0596
//! use carina_core::{differ::PendingCascadePlan, effect::Effect};
//! fn append(builder: PendingCascadePlan, forged: Effect) {
//!     let _ = builder.finish(std::iter::empty(), |plan| plan.add(forged));
//! }
//! ```
//!
//! Saved replay has no direct Plan conversion or mutable effect access. Saved
//! files remain user input and can be deserialized; the CLI verifies them:
//!
//! ```compile_fail,E0277
//! use carina_core::{plan::Plan, saved_replay::SavedPlanReplay, resource::SavedModuleSource};
//! let raw: Plan<SavedModuleSource> = Plan::new().into();
//! let replay: SavedPlanReplay = raw.into();
//! ```
//!
//! ```compile_fail,E0596
//! use carina_core::{saved_replay::SavedPlanReplay, effect::Effect, resource::SavedModuleSource};
//! fn append(replay: &mut SavedPlanReplay, effect: Effect<SavedModuleSource>) { replay.add(effect); }
//! ```
//!
//! Struct literals cannot access the private row fields. Typed state owners
//! supply their values; private fields do not authenticate those values:
//!
//! ```compile_fail,E0451
//! use carina_core::{planning_state::PlanningStateRow, provider::AppliedIdentifier, resource::ResolvedResourceId};
//! fn forged(id: ResolvedResourceId, identifier: AppliedIdentifier) -> PlanningStateRow {
//!     PlanningStateRow { id, identifier: Some(identifier.clone()), persisted_identifier: Some(identifier), current_kind: carina_core::planning_state::PlanningCurrentKind::Applied,
//!         binding: None, directives: Default::default(), dependency_bindings: Default::default(), deposed: Vec::new() }
//! }
//! ```
//!
//! The backend constructor has no separate bucket-field selector. It reads the
//! bucket field of the supplied configuration, whose authenticity the CLI checks:
//!
//! ```compile_fail,E0061
//! use carina_core::{resolved_graph::ResolvedInitialDeletePlan, executor::prepare_initial_delete,
//!     parser::ExpandedInferredFile, schema::SchemaRegistry};
//! fn forged(configuration: &ExpandedInferredFile, schemas: &SchemaRegistry) {
//!     let plan = ResolvedInitialDeletePlan::from_backend_configuration(configuration, "p", "Bucket", "key", schemas)
//!         .unwrap().admit(schemas).unwrap();
//!     let _ = prepare_initial_delete(&plan);
//! }
//! ```
