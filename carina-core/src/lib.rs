//! Carina Core
//!
//! Core library for an infrastructure management tool that treats side effects as values

pub use carina_provider_protocol::{FiniteF64, NonFiniteFloatError};

mod environment;
mod user_dirs;
mod value_description;

pub use environment::EnvironmentSnapshot;
#[cfg(any(test, feature = "test-utils"))]
pub use user_dirs::TestUserDirs;
pub use user_dirs::{UserDirs, UserDirsError};
pub use value_description::ValueDescription;

pub mod binding_index;
#[cfg(test)]
mod binding_index_split_tests;
pub mod builtins;
pub mod config_loader;
pub mod data_source_value;
pub mod deps;
pub mod desired;
pub mod detail_rows;
pub mod diff_helpers;
pub mod differ;
pub mod effect;
pub(crate) mod eval_value;
pub mod executor;
pub mod explicit;
pub mod formatter;
pub mod heredoc;
pub mod hint;
pub mod identifier;
pub mod integer_arithmetic;
#[doc(hidden)]
pub mod json_field_order;
pub mod keywords;
pub mod lint;
pub mod module;
pub mod module_resolver;
pub mod name_override;
pub(crate) mod non_empty;
pub(crate) mod numeric;
pub mod parser;
pub mod paths;
pub mod plan;
pub mod plan_tree;
pub mod protocol_convert;
pub mod provider;
pub mod provider_source;
pub mod provider_text;
pub mod provider_wire;
pub mod references;
pub mod resolved_graph;
pub mod resolver;
#[cfg(test)]
mod resolver_split_tests;
pub mod resource;
pub mod schema;
pub mod shutdown;
pub mod static_type;
pub mod upstream_exports;
pub mod utils;
pub mod validation;
pub mod value;
pub mod version_constraint;
pub mod wait;

#[cfg(test)]
pub(crate) fn finite(value: f64) -> carina_provider_protocol::FiniteF64 {
    carina_provider_protocol::FiniteF64::new(value).unwrap()
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::{
        effect::DeferredCreatePayload,
        parser::{ExpandedDeferredForExpression, ExpandedParsedFile},
        resolved_graph::resolve_expanded_graph,
        resource::{RelativeBindingName, Resource, RootScope},
    };
    use std::collections::HashMap;

    pub fn snapshot_attributes<P: crate::resource::StateValueLeaf>(
        values: impl IntoIterator<Item = (String, crate::resource::Value<P>)>,
    ) -> HashMap<String, crate::resource::SnapshotValue> {
        values
            .into_iter()
            .map(|(key, value)| (key, crate::resource::state_value_snapshot(&value)))
            .collect()
    }

    pub fn available_value(
        value: &crate::resource::SnapshotValue,
    ) -> crate::resource::ExpandedValue {
        value
            .to_value_at("test fixture")
            .expect("fixture must contain available values")
    }

    pub fn available_state(state: crate::resource::SnapshotState) -> crate::resource::State {
        crate::resource::State {
            id: state.id,
            identifier: state.identifier,
            attributes: available_attributes(state.attributes),
            exists: state.exists,
            dependency_bindings: state.dependency_bindings,
            partial_read: state.partial_read,
        }
    }

    pub fn available_attributes(
        values: impl IntoIterator<Item = (String, crate::resource::SnapshotValue)>,
    ) -> HashMap<String, crate::resource::ExpandedValue> {
        values
            .into_iter()
            .map(|(key, value)| (key, available_value(&value)))
            .collect()
    }

    pub fn lift_state_enum_leaves(
        values: &mut HashMap<String, crate::resource::ExpandedValue>,
        schema: &crate::schema::CompiledResourceSchema,
    ) {
        let mut snapshot = snapshot_attributes(std::mem::take(values));
        crate::utils::lift_state_enum_leaves(&mut snapshot, schema);
        *values = available_attributes(snapshot);
    }

    pub fn raw_saved_attrs<P: crate::resource::StateValueLeaf>(
        values: HashMap<crate::resource::ResourceId, HashMap<String, crate::resource::Value<P>>>,
    ) -> crate::provider::RawSavedAttrs {
        crate::provider::RawSavedAttrs::from_decoded_values_for_test(
            values
                .into_iter()
                .map(|(id, values)| (id, snapshot_attributes(values)))
                .collect(),
        )
    }

    pub fn snapshot_plan(plan: &crate::plan::Plan) -> crate::plan::SnapshotPlan {
        crate::value::redact_secrets_in_plan(plan).expect("fixture plan must redact")
    }

    pub fn snapshot_effect<S: Clone>(
        effect: &crate::effect::Effect<S>,
    ) -> crate::effect::Effect<S, crate::resource::SnapshotLeaf> {
        crate::value::redact_secrets_in_effect(effect).expect("fixture effect must redact")
    }
    /// Explicit rows shared by scheduler/display deferred-loop fixtures.
    pub fn compiled_loop(template: Box<ExpandedDeferredForExpression>) -> DeferredCreatePayload {
        let root = RootScope::new();
        let configuration = ExpandedParsedFile {
            upstream_states: vec![crate::parser::UpstreamState {
                binding: root.resolve("upstream".into()),
                source: "upstream".into(),
            }],
            resources: ["cert", "primary", "fallback", "parent"]
                .into_iter()
                .map(|binding| {
                    Resource::new("test", binding)
                        .with_absolute_binding(root.resolve(RelativeBindingName::new(binding)))
                })
                .collect(),
            ..Default::default()
        };
        let graph = resolve_expanded_graph(configuration.into())
            .expect("deferred fixture rows form a valid graph");
        let (template, dependencies) = graph
            .compile_deferred_for(*template)
            .expect("deferred fixture dependencies resolve against its declared producer rows");
        DeferredCreatePayload::new(Box::new(template), dependencies)
    }
    pub fn compiled_loop_with_producers(
        template: Box<ExpandedDeferredForExpression>,
        resources: Vec<crate::resource::ExpandedResource>,
    ) -> DeferredCreatePayload {
        let configuration = ExpandedParsedFile {
            resources,
            ..Default::default()
        };
        let graph =
            resolve_expanded_graph(configuration.into()).expect("fixture producers form a graph");
        let (template, dependencies) = graph
            .compile_deferred_for(*template)
            .expect("fixture loop dependencies");
        DeferredCreatePayload::new(Box::new(template), dependencies)
    }
    pub fn persisted(
        rows: impl IntoIterator<Item = crate::resource::ResolvedResourceId>,
    ) -> crate::resource::PersistedDestroyDependencies {
        crate::resource::PersistedDestroyDependencies::from_test_dependencies(rows)
    }
    pub fn resolved_targets(
        targets: crate::differ::DeferredCreateTargets,
    ) -> crate::differ::ResolvedDeferredCreateTargets {
        let root = RootScope::new();
        let resources = ["cert", "primary", "fallback", "parent"]
            .into_iter()
            .map(|binding| {
                Resource::new("test", binding)
                    .with_absolute_binding(root.resolve(RelativeBindingName::new(binding)))
            })
            .collect::<Vec<_>>();
        let configuration = ExpandedParsedFile {
            resources,
            upstream_states: vec![crate::parser::UpstreamState {
                binding: root.resolve("upstream".into()),
                source: "upstream".into(),
            }],
            ..Default::default()
        };
        let graph =
            resolve_expanded_graph(configuration.into()).expect("fixture producers resolve");
        let (_, index) = graph.into_parts();
        targets
            .resolve_graph(&index)
            .expect("fixture deferred targets resolve")
    }
    pub fn compiled_replace(
        deletes: crate::effect::NonEmptyDeletes,
        template: Box<ExpandedDeferredForExpression>,
    ) -> crate::effect::DeferredReplacePayload {
        let payload = compiled_loop(template);
        crate::effect::DeferredReplacePayload {
            deletes,
            template: payload.template,
            dependency_template: payload.dependency_template,
        }
    }
}
