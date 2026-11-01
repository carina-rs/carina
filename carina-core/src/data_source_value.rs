//! Total, typed outcomes for configured data-source reads.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::resource::{
    AbsoluteBindingAddress, AccessPath, ApplyTimeUnknown, DataSource, ModuleSource, ResourceId,
    State,
};

/// A data-source input attribute whose unresolved value shapes make the read
/// apply-time dependent or unsafe to hand to a provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedDataSourceInput {
    pub attribute: String,
    pub paths: Vec<AccessPath>,
    pub bindings: Vec<AbsoluteBindingAddress>,
    pub unknowns: Vec<ApplyTimeUnknown>,
}

/// The value status of one configured data source after the pre-apply read
/// phase has classified every query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DataSourceValue {
    /// The provider query completed and returned this transient state.
    Read(Box<State>),
    /// The query depends on a value that will only exist during apply.
    DeferredToApply(Vec<UnresolvedDataSourceInput>),
    /// Planning already has a structured error that forbids provider
    /// dispatch, so this query was deliberately not executed.
    BlockedByPlanError,
}

impl DataSourceValue {
    pub fn read(state: State) -> Self {
        Self::Read(Box::new(state))
    }

    pub fn deferred_to_apply(unresolved: Vec<UnresolvedDataSourceInput>) -> Self {
        Self::DeferredToApply(unresolved)
    }

    pub fn blocked_by_plan_error() -> Self {
        Self::BlockedByPlanError
    }
}

/// One outcome for every data source in one configuration.
///
/// The parallel vectors are private and every constructor evaluates its
/// callback once per configured data source. Consumers therefore iterate
/// paired `(DataSource, DataSourceValue)` entries and cannot observe a query
/// that is absent from both the read-state and deferred sets.
#[derive(Clone, Debug, PartialEq)]
pub struct DataSourceValues<S = ModuleSource, D = crate::resource::UnexpandedDependencyFields> {
    data_sources: Vec<DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, D>>,
    values: Vec<DataSourceValue>,
}

impl<S: Clone, D: Clone> DataSourceValues<S, D> {
    /// Build a total collection by classifying every configured data source.
    pub fn from_data_sources(
        data_sources: &[crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >],
        mut value_for: impl FnMut(
            &crate::resource::DataSource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                D,
            >,
        ) -> DataSourceValue,
    ) -> Self {
        Self {
            data_sources: data_sources.to_vec(),
            values: data_sources.iter().map(&mut value_for).collect(),
        }
    }

    /// Fallible counterpart used at serialization and provider-I/O
    /// boundaries. An error aborts construction; it cannot produce a partial
    /// collection.
    pub fn try_from_data_sources<E>(
        data_sources: &[crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >],
        mut value_for: impl FnMut(
            &crate::resource::DataSource<
                S,
                crate::resource::NoPlaceholder,
                crate::resource::AbsoluteBindingAddress,
                D,
            >,
        ) -> Result<DataSourceValue, E>,
    ) -> Result<Self, E> {
        let values = data_sources
            .iter()
            .map(&mut value_for)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            data_sources: data_sources.to_vec(),
            values,
        })
    }

    /// The total collection for a configuration with no data sources.
    pub fn empty() -> Self {
        Self {
            data_sources: Vec::new(),
            values: Vec::new(),
        }
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<
        Item = (
            &DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, D>,
            &DataSourceValue,
        ),
    > {
        self.data_sources.iter().zip(&self.values)
    }

    pub fn data_sources(
        &self,
    ) -> &[DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, D>] {
        &self.data_sources
    }

    pub fn read_states(&self) -> impl Iterator<Item = (&ResourceId, &State)> {
        self.iter().filter_map(|(data_source, value)| match value {
            DataSourceValue::Read(state) => Some((&data_source.id, state.as_ref())),
            DataSourceValue::DeferredToApply(_) | DataSourceValue::BlockedByPlanError => None,
        })
    }

    pub fn iter_read_states_mut(&mut self) -> impl Iterator<Item = (&ResourceId, &mut State)> {
        self.data_sources.iter().zip(&mut self.values).filter_map(
            |(data_source, value)| match value {
                DataSourceValue::Read(state) => Some((&data_source.id, state.as_mut())),
                DataSourceValue::DeferredToApply(_) | DataSourceValue::BlockedByPlanError => None,
            },
        )
    }

    pub fn read_states_map(&self) -> HashMap<ResourceId, State> {
        self.read_states()
            .map(|(id, state)| (id.clone(), state.clone()))
            .collect()
    }

    pub fn into_read_states(self) -> HashMap<ResourceId, State> {
        self.data_sources
            .into_iter()
            .zip(self.values)
            .filter_map(|(data_source, value)| match value {
                DataSourceValue::Read(state) => Some((data_source.id, *state)),
                DataSourceValue::DeferredToApply(_) | DataSourceValue::BlockedByPlanError => None,
            })
            .collect()
    }

    pub fn deferred_reads(
        &self,
    ) -> impl Iterator<Item = (&ResourceId, &[UnresolvedDataSourceInput])> {
        self.iter().filter_map(|(data_source, value)| match value {
            DataSourceValue::Read(_) => None,
            DataSourceValue::DeferredToApply(unresolved) => {
                Some((&data_source.id, unresolved.as_slice()))
            }
            DataSourceValue::BlockedByPlanError => None,
        })
    }

    /// Merge successful executor reads into their existing typed outcomes.
    ///
    /// This is intentionally a partial update: a cancelled or failed apply can
    /// leave some outcomes deferred. Unknown ids are rejected rather than
    /// manufacturing a new collection entry.
    pub fn merge_read_states(
        &mut self,
        states: &HashMap<ResourceId, State>,
    ) -> Result<(), DataSourceReadStateMergeError> {
        let configured = self
            .data_sources
            .iter()
            .map(|data_source| &data_source.id)
            .collect::<HashSet<_>>();
        if let Some(id) = states.keys().find(|id| !configured.contains(id)) {
            return Err(DataSourceReadStateMergeError::Unexpected(id.clone()));
        }

        for (data_source, value) in self.data_sources.iter().zip(&mut self.values) {
            if let Some(state) = states.get(&data_source.id) {
                *value = DataSourceValue::read(state.clone());
            }
        }
        Ok(())
    }

    /// Cross the executor boundary only after planning has eliminated every
    /// blocked outcome. The returned type has no `BlockedByPlanError` variant,
    /// so execution cannot silently reinterpret a blocked read as pre-read.
    pub fn into_executable(
        self,
    ) -> Result<ExecutableDataSourceValues<S>, BlockedDataSourceValuesError> {
        let mut blocked = Vec::new();
        let values = self
            .data_sources
            .iter()
            .zip(self.values)
            .filter_map(|(data_source, value)| match value {
                DataSourceValue::Read(state) => Some(ExecutableDataSourceValue::Read(state)),
                DataSourceValue::DeferredToApply(unresolved) => {
                    Some(ExecutableDataSourceValue::DeferredToApply(unresolved))
                }
                DataSourceValue::BlockedByPlanError => {
                    blocked.push(data_source.id.clone());
                    None
                }
            })
            .collect::<Vec<_>>();
        if !blocked.is_empty() {
            return Err(BlockedDataSourceValuesError { blocked });
        }
        Ok(ExecutableDataSourceValues {
            data_sources: self
                .data_sources
                .into_iter()
                .map(DataSource::into_value_snapshot)
                .collect(),
            values,
        })
    }
}

/// Executor-safe outcome for one configured data source.
#[derive(Clone, Debug, PartialEq)]
enum ExecutableDataSourceValue {
    Read(Box<State>),
    DeferredToApply(Vec<UnresolvedDataSourceInput>),
}

/// Total data-source outcomes after structured plan errors have been handled.
///
/// Unlike [`DataSourceValues`], this type cannot represent
/// [`DataSourceValue::BlockedByPlanError`]. It is the only data-source input
/// accepted by the executor.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutableDataSourceValues<S = ModuleSource> {
    data_sources: Vec<DataSource<S, crate::resource::NoPlaceholder, AbsoluteBindingAddress, ()>>,
    values: Vec<ExecutableDataSourceValue>,
}

impl<S: Clone> ExecutableDataSourceValues<S> {
    pub fn empty() -> Self {
        DataSourceValues::<S>::empty()
            .into_executable()
            .expect("an empty collection cannot contain a blocked outcome")
    }

    pub fn read_states_map(&self) -> HashMap<ResourceId, State> {
        self.data_sources
            .iter()
            .zip(&self.values)
            .filter_map(|(data_source, value)| match value {
                ExecutableDataSourceValue::Read(state) => {
                    Some((data_source.id.clone(), state.as_ref().clone()))
                }
                ExecutableDataSourceValue::DeferredToApply(_) => None,
            })
            .collect()
    }

    pub fn deferred_inputs(&self, id: &ResourceId) -> Option<&[UnresolvedDataSourceInput]> {
        self.data_sources
            .iter()
            .position(|data_source| &data_source.id == id)
            .and_then(|index| match &self.values[index] {
                ExecutableDataSourceValue::Read(_) => None,
                ExecutableDataSourceValue::DeferredToApply(unresolved) => {
                    Some(unresolved.as_slice())
                }
            })
    }

    pub fn has_deferred_reads(&self) -> bool {
        self.values
            .iter()
            .any(|value| matches!(value, ExecutableDataSourceValue::DeferredToApply(_)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("data-source outcomes blocked by plan errors: {blocked:?}")]
pub struct BlockedDataSourceValuesError {
    blocked: Vec<ResourceId>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DataSourceReadStateMergeError {
    #[error("executor returned an unknown data-source state `{0}`")]
    Unexpected(ResourceId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{
        AbsoluteBindingAddress, ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value,
        RelativeBindingName, RootScope,
    };

    fn absolute_binding(binding: &str) -> AbsoluteBindingAddress {
        RootScope::new().resolve(RelativeBindingName::new(binding))
    }

    fn data_source(binding: &str) -> DataSource {
        let identity = crate::resource::ResourceIdentity::try_from(binding.to_string())
            .expect("test identity");
        let mut data_source = DataSource::with_provider("mock", "iam.Roles", identity, None);
        data_source.binding = Some(absolute_binding(binding));
        data_source
    }

    #[test]
    fn construction_classifies_every_configured_data_source() {
        let data_sources = [data_source("read_now"), data_source("read_later")];
        let mut visited = Vec::new();
        let values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
            visited.push(data_source.id.clone());
            if data_source
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("read_now")
            {
                DataSourceValue::read(State::existing(
                    data_source.id.clone(),
                    HashMap::from([(
                        "names".to_string(),
                        Value::Concrete(ConcreteValue::List(Vec::new())),
                    )]),
                ))
            } else {
                DataSourceValue::deferred_to_apply(Vec::new())
            }
        });

        let expected_ids = data_sources
            .iter()
            .map(|data_source| data_source.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(visited, expected_ids);
        assert_eq!(values.iter().count(), 2);
        assert_eq!(values.read_states().count(), 1);
        assert!(matches!(
            values.iter().nth(1).map(|(_, value)| value),
            Some(DataSourceValue::DeferredToApply(_))
        ));
    }

    #[test]
    fn fallible_construction_never_returns_a_partial_collection() {
        let data_sources = [data_source("one"), data_source("two")];
        let result = DataSourceValues::try_from_data_sources(&data_sources, |data_source| {
            if data_source
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("two")
            {
                Err("second query failed")
            } else {
                Ok(DataSourceValue::deferred_to_apply(Vec::new()))
            }
        });

        assert_eq!(result, Err("second query failed"));
    }

    #[test]
    fn apply_time_read_updates_deferred_outcome_in_place() {
        let data_sources = [data_source("read_now"), data_source("read_later")];
        let deferred = UnresolvedDataSourceInput {
            attribute: "name_regex".to_string(),
            paths: vec![AccessPath::new("producer", "name").resolve_root(&RootScope::new())],
            bindings: vec![absolute_binding("producer")],
            unknowns: Vec::new(),
        };
        let mut values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
            if data_source
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("read_now")
            {
                DataSourceValue::read(State::existing(data_source.id.clone(), HashMap::new()))
            } else {
                DataSourceValue::deferred_to_apply(vec![deferred.clone()])
            }
        });
        let applied_state = State::existing(
            data_sources[1].id.clone(),
            HashMap::from([(
                "names".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("resolved".into()),
                )])),
            )]),
        );

        values
            .merge_read_states(&HashMap::from([(
                data_sources[1].id.clone(),
                applied_state.clone(),
            )]))
            .expect("an apply-time read must update its configured outcome");

        assert_eq!(
            values
                .read_states()
                .find_map(|(id, state)| (id == &data_sources[1].id).then_some(state)),
            Some(&applied_state)
        );
        assert_eq!(values.read_states().count(), 2);
    }

    #[test]
    fn read_state_merge_rejects_an_unknown_data_source() {
        let data_sources = [data_source("read_now"), data_source("read_later")];
        let mut values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
            if data_source
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("read_now")
            {
                DataSourceValue::read(State::not_found(data_source.id.clone()))
            } else {
                DataSourceValue::deferred_to_apply(Vec::new())
            }
        });
        let unexpected = data_source("unexpected").id;

        assert_eq!(
            values.merge_read_states(&HashMap::from([(
                unexpected.clone(),
                State::not_found(unexpected.clone()),
            )])),
            Err(DataSourceReadStateMergeError::Unexpected(unexpected)),
        );
        assert_eq!(values.read_states().count(), 1);
    }

    #[test]
    fn executable_values_preserve_read_and_deferred_outcomes() {
        let data_sources = [data_source("read_now"), data_source("read_later")];
        let deferred = UnresolvedDataSourceInput {
            attribute: "name_regex".to_string(),
            paths: vec![AccessPath::new("producer", "name").resolve_root(&RootScope::new())],
            bindings: vec![absolute_binding("producer")],
            unknowns: Vec::new(),
        };
        let values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
            if data_source.id == data_sources[0].id {
                DataSourceValue::read(State::existing(data_source.id.clone(), HashMap::new()))
            } else {
                DataSourceValue::deferred_to_apply(vec![deferred.clone()])
            }
        });

        let executable = values
            .into_executable()
            .expect("read and deferred outcomes are executable");

        assert!(
            executable
                .read_states_map()
                .contains_key(&data_sources[0].id)
        );
        assert_eq!(
            executable.deferred_inputs(&data_sources[1].id),
            Some(std::slice::from_ref(&deferred)),
        );
    }

    #[test]
    fn blocked_outcome_cannot_become_executable() {
        let data_sources = [data_source("blocked")];
        let values = DataSourceValues::from_data_sources(&data_sources, |_| {
            DataSourceValue::blocked_by_plan_error()
        });

        let error = values
            .into_executable()
            .expect_err("a plan-error-blocked read must not cross the executor boundary");

        assert_eq!(
            error,
            BlockedDataSourceValuesError {
                blocked: vec![data_sources[0].id.clone()],
            }
        );
    }
}
