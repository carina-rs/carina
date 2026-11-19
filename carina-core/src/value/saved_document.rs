use super::*;
use crate::resource::{AbsoluteBindingAddress, PlanValueKind, StateValueLeaf};
use std::ops::Deref;

/// A saved-document tree whose values passed retained-call admission.
/// No mutable view or unchecked constructor is available.
///
/// ```compile_fail,E0423
/// use carina_core::{resource::Resource, value::Saved};
/// fn bypass(resource: Resource) {
///     let _ = Saved(resource.to_snapshot());
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Saved<T>(T);
impl<T> Saved<T> {
    pub fn as_inner(&self) -> &T {
        &self.0
    }
    pub fn into_inner(self) -> T {
        self.0
    }
}
impl<T> Deref for Saved<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<T: serde::Serialize> serde::Serialize for Saved<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}
impl<'de, T: serde::Deserialize<'de> + DocumentTree> serde::Deserialize<'de> for Saved<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        admit(T::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// An admitted pre-resolution resource tree, distinct from evaluated snapshots.
///
/// ```compile_fail,E0423
/// use carina_core::{resource::Resource, value::SavedAuthored};
/// fn bypass(resource: Resource) {
///     let _ = SavedAuthored(resource.to_snapshot());
/// }
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::{resource::Resource, value::{Saved, SavedAuthored}};
/// fn bypass(evaluated: Saved<Resource>) -> SavedAuthored<Resource> {
///     evaluated.into()
/// }
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::{resource::Resource, value::SavedDocumentConversion};
/// fn bypass(conversion: &SavedDocumentConversion, evaluated: &Resource) {
///     let _ = conversion.authored_resource(evaluated, evaluated);
/// }
/// ```
///
/// ```compile_fail,E0596
/// use carina_core::{resource::Resource, value::SavedAuthored};
/// fn mutate(mut authored: SavedAuthored<Resource>) {
///     authored.attributes.clear();
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SavedAuthored<T>(T);

impl<T> SavedAuthored<T> {
    pub fn as_inner(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Deref for SavedAuthored<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: serde::Serialize> serde::Serialize for SavedAuthored<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: serde::Deserialize<'de> + DocumentTree> serde::Deserialize<'de> for SavedAuthored<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        admit(T::deserialize(deserializer)?)
            .map(|value| Self(value.into_inner()))
            .map_err(serde::de::Error::custom)
    }
}

impl<S, P: PlanValueKind>
    SavedAuthored<crate::resource::Resource<S, P, AbsoluteBindingAddress, ()>>
{
    pub fn into_saved_source(
        self,
    ) -> SavedAuthored<
        crate::resource::Resource<
            crate::resource::SavedModuleSource,
            P,
            AbsoluteBindingAddress,
            (),
        >,
    >
    where
        crate::resource::Resource<S, P, AbsoluteBindingAddress, ()>: Into<
            crate::resource::Resource<
                crate::resource::SavedModuleSource,
                P,
                AbsoluteBindingAddress,
                (),
            >,
        >,
    {
        SavedAuthored(self.0.into())
    }
}

impl<S: Clone>
    SavedAuthored<crate::resource::Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>
{
    pub fn to_value_resource(
        &self,
    ) -> Result<
        SavedAuthored<
            crate::resource::Resource<
                S,
                crate::resource::NoPlaceholder,
                AbsoluteBindingAddress,
                (),
            >,
        >,
        crate::resource::UnavailableSecretError,
    > {
        self.0.to_value_resource().map(SavedAuthored)
    }

    pub fn rehydrate_resource_secrets<T, E>(
        &self,
        evaluated: &crate::resource::Resource<
            T,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        SavedAuthored<
            crate::resource::Resource<
                S,
                crate::resource::NoPlaceholder,
                AbsoluteBindingAddress,
                (),
            >,
        >,
        SecretRehydrationSiteError,
    > {
        rehydrate_resource_secrets(&self.0, evaluated).map(SavedAuthored)
    }
}

pub(crate) mod sealed {
    pub trait Tree {
        fn visit(&self, names: &mut super::FunctionNames);
    }
}
/// Closed inventory of value-bearing saved-document trees.
pub trait DocumentTree: sealed::Tree {}
impl<T: sealed::Tree + ?Sized> DocumentTree for T {}
#[derive(Default)]
pub struct FunctionNames(std::collections::BTreeMap<crate::parser::DeclarationLocation, String>);
impl FunctionNames {
    pub(crate) fn tree<T: DocumentTree + ?Sized>(&mut self, value: &T) {
        value.visit(self);
    }
    pub(crate) fn value<P: ValueKind, B: crate::resource::BindingAddress>(
        &mut self,
        value: &Value<P, B>,
    ) {
        crate::function_evaluation::visit_deferred_user_function_calls(value, &mut |function| {
            self.0
                .entry(function.body.location.clone())
                .or_insert_with(|| function.name.clone());
        });
    }
    fn finish(self) -> Result<(), SerializationError> {
        match RetainedUserFunctionCallError::from_functions(self.0) {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }
}

fn admit<T: DocumentTree>(value: T) -> Result<Saved<T>, SerializationError> {
    let mut names = FunctionNames::default();
    value.visit(&mut names);
    names.finish()?;
    Ok(Saved(value))
}

/// Borrowed conversion authority minted by the document constructor after exports.
///
/// ```compile_fail,E0423
/// let conversion = carina_core::value::SavedDocumentConversion;
/// ```
pub struct SavedDocumentConversion {
    _private: (),
}
impl SavedDocumentConversion {
    pub fn document<E, T, Error>(
        parsed: &crate::parser::ExpandedFile<E>,
        sources: &[&dyn DocumentTree],
        build: impl FnOnce(&Self) -> Result<T, Error>,
    ) -> Result<T, Error>
    where
        E: crate::parser::ExportParamLike<crate::resource::NoPlaceholder, AbsoluteBindingAddress>,
        Error: From<SerializationError>,
    {
        let roots = DocumentRoots {
            exports: parsed
                .export_params
                .iter()
                .filter_map(|export| export.value())
                .collect(),
            sources,
        };
        admit(roots)?;
        build(&Self { _private: () })
    }
    pub fn snapshot<T: DocumentTree>(&self, value: T) -> Result<Saved<T>, SerializationError> {
        admit(value)
    }
    fn convert<T: DocumentTree, U: DocumentTree>(
        &self,
        value: &T,
        convert: impl FnOnce(&T) -> Result<U, SerializationError>,
    ) -> Result<Saved<U>, SerializationError> {
        let source = admit(value)?;
        admit(convert(source.as_inner())?)
    }
    pub fn value(&self, value: &Value) -> Result<Saved<SnapshotValue>, SerializationError> {
        self.snapshot(value.to_snapshot())
    }
    pub fn redact_secrets_in_value(
        &self,
        value: &Value,
    ) -> Result<Saved<SnapshotValue>, SerializationError> {
        self.convert(value, redact_secrets_in_value)
    }
    pub fn redact_secrets_in_attributes(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<Saved<BTreeMap<String, SnapshotValue>>, SerializationError> {
        self.convert(attributes, redact_secrets_in_attributes)
    }
    pub fn redact_secrets_in_saved_composition(
        &self,
        composition: &crate::resource::SavedComposition,
    ) -> Result<Saved<crate::resource::SavedComposition>, SerializationError> {
        self.convert(composition, redact_secrets_in_saved_composition)
    }
    pub fn authored_resource<S: Clone, T, E>(
        &self,
        source: &crate::effect::deps::UnresolvedResource<S>,
        evaluated: &crate::resource::Resource<
            T,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        SavedAuthored<crate::resource::Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>,
        SerializationError,
    > {
        self.redact_secrets_in_resource_using_evaluated(source.as_resource(), evaluated)
            .map(|saved| SavedAuthored(saved.into_inner()))
    }
    pub fn state_value<P: StateValueLeaf>(
        &self,
        value: &Value<P>,
    ) -> Result<Saved<SnapshotValue>, SerializationError> {
        self.convert(value, |value| {
            redact_state_snapshot_value(&crate::resource::state_value_snapshot(value))
        })
    }
    pub fn redact_secrets_in_resource<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >,
    ) -> Result<
        Saved<crate::resource::Resource<S, SnapshotLeaf, AbsoluteBindingAddress, D>>,
        SerializationError,
    > {
        self.convert(resource, redact_secrets_in_resource)
    }
    pub fn redact_secrets_in_resource_reusing_persisted_hashes<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >,
        persisted: &crate::provider::PersistedAttributes,
    ) -> Result<
        Saved<crate::resource::Resource<S, SnapshotLeaf, AbsoluteBindingAddress, D>>,
        SerializationError,
    > {
        self.convert(resource, |resource| {
            redact_secrets_in_resource_reusing_persisted_hashes(resource, persisted)
        })
    }
    pub fn redact_secrets_in_resource_using_evaluated<S: Clone, D: Clone, T, E>(
        &self,
        source: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >,
        evaluated: &crate::resource::Resource<
            T,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        Saved<crate::resource::Resource<S, SnapshotLeaf, AbsoluteBindingAddress, D>>,
        SerializationError,
    > {
        self.convert(source, |source| {
            redact_secrets_in_resource_using_evaluated(source, evaluated)
        })
    }
    pub fn redact_secrets_in_data_source<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >,
    ) -> Result<
        Saved<crate::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, D>>,
        SerializationError,
    > {
        self.convert(resource, redact_secrets_in_data_source)
    }
    pub fn redact_secrets_in_data_source_using_evaluated<S: Clone, D: Clone, T, E>(
        &self,
        source: &crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            D,
        >,
        evaluated: &crate::resource::DataSource<
            T,
            crate::resource::NoPlaceholder,
            AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        Saved<crate::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, D>>,
        SerializationError,
    > {
        self.convert(source, |source| {
            redact_secrets_in_data_source_using_evaluated(source, evaluated)
        })
    }
    pub fn redact_secrets_in_live_composition(
        &self,
        resource: &crate::resource::LiveComposition,
    ) -> Result<Saved<crate::resource::LiveComposition<SnapshotLeaf>>, SerializationError> {
        self.convert(resource, redact_secrets_in_live_composition)
    }
    pub fn redact_secrets_in_state<P: StateValueLeaf>(
        &self,
        state: &crate::resource::State<P>,
    ) -> Result<Saved<crate::resource::SnapshotState>, SerializationError> {
        self.convert(state, redact_secrets_in_state_impl)
    }
    pub fn redact_secrets_in_plan_with_desired_and_historical<D>(
        &self,
        plan: &crate::plan::Plan,
        desired: &HashMap<
            crate::resource::ResourceId,
            Saved<
                crate::resource::Resource<
                    crate::resource::ModuleSource,
                    SnapshotLeaf,
                    AbsoluteBindingAddress,
                    D,
                >,
            >,
        >,
        historical: &HashMap<crate::resource::ResourceId, Saved<crate::resource::SnapshotState>>,
    ) -> Result<Saved<crate::plan::SnapshotPlan>, SerializationError>
    where
        D: Clone,
    {
        let desired = desired
            .iter()
            .map(|(key, value)| (key.clone(), value.as_inner().clone()))
            .collect();
        let historical = historical
            .iter()
            .map(|(key, value)| (key.clone(), value.as_inner().clone()))
            .collect();
        self.convert(plan, |plan| {
            redact_secrets_in_plan_with_desired_and_historical(plan, &desired, &historical)
        })
    }
    pub fn provider_configs<P: PlanValueKind>(
        &self,
        configs: &crate::parser::SavedProviderConfigs<P>,
    ) -> Result<Saved<crate::parser::SavedProviderConfigs>, SerializationError> {
        self.convert(configs, |configs| {
            let values = configs
                .as_provider_configs()
                .iter()
                .map(|config| {
                    config.try_map_value_stage(&mut |value| {
                        redact_state_snapshot_value(&crate::resource::state_value_snapshot(value))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(crate::parser::SavedProviderConfigs::from_snapshot_configs(
                values,
            ))
        })
    }
}
struct DocumentRoots<'a> {
    exports: Vec<&'a Value>,
    sources: &'a [&'a dyn DocumentTree],
}
impl sealed::Tree for DocumentRoots<'_> {
    fn visit(&self, names: &mut FunctionNames) {
        for value in &self.exports {
            names.value(value);
        }
        for source in self.sources {
            source.visit(names);
        }
    }
}
impl<P: ValueKind, B: crate::resource::BindingAddress> sealed::Tree for Value<P, B> {
    fn visit(&self, names: &mut FunctionNames) {
        names.value(self);
    }
}
impl<T: DocumentTree> sealed::Tree for &T {
    fn visit(&self, names: &mut FunctionNames) {
        sealed::Tree::visit(*self, names);
    }
}
impl<T: DocumentTree> sealed::Tree for SavedAuthored<T> {
    fn visit(&self, names: &mut FunctionNames) {
        names.tree(self.as_inner());
    }
}
impl<T: DocumentTree> sealed::Tree for Vec<T> {
    fn visit(&self, names: &mut FunctionNames) {
        for value in self {
            value.visit(names);
        }
    }
}
impl<T: DocumentTree> sealed::Tree for Option<T> {
    fn visit(&self, names: &mut FunctionNames) {
        if let Some(value) = self {
            value.visit(names);
        }
    }
}
impl<K, V: DocumentTree> sealed::Tree for HashMap<K, V> {
    fn visit(&self, names: &mut FunctionNames) {
        self.values().for_each(|value| value.visit(names));
    }
}
impl<K, V: DocumentTree> sealed::Tree for BTreeMap<K, V> {
    fn visit(&self, names: &mut FunctionNames) {
        self.values().for_each(|value| value.visit(names));
    }
}
impl<K, V: DocumentTree> sealed::Tree for IndexMap<K, V> {
    fn visit(&self, names: &mut FunctionNames) {
        self.values().for_each(|value| value.visit(names));
    }
}
impl<S, P: PlanValueKind, D> sealed::Tree
    for crate::resource::Resource<S, P, AbsoluteBindingAddress, D>
{
    fn visit(&self, names: &mut FunctionNames) {
        let Self {
            id: _,
            attributes,
            authored_slots: _,
            directives: _,
            resolved_ignore_changes: _,
            prefixes: _,
            binding: _,
            dependencies: _,
            module_source: _,
            default_tag_keys: _,
            loop_provenance: _,
            quoted_string_attrs: _,
            declaration: _,
        } = self;
        attributes.values().for_each(|value| names.value(value));
    }
}
impl<S, P: PlanValueKind, D> sealed::Tree
    for crate::resource::DataSource<S, P, AbsoluteBindingAddress, D>
{
    fn visit(&self, names: &mut FunctionNames) {
        let Self {
            id: _,
            attributes,
            authored_slots: _,
            directives: _,
            binding: _,
            dependencies: _,
            module_source: _,
            quoted_string_attrs: _,
            declaration: _,
        } = self;
        attributes.values().for_each(|value| names.value(value));
    }
}
impl<P: StateValueLeaf> sealed::Tree for crate::resource::State<P> {
    fn visit(&self, names: &mut FunctionNames) {
        let Self {
            id: _,
            identifier: _,
            attributes,
            exists: _,
            dependency_bindings: _,
            partial_read: _,
        } = self;
        attributes.values().for_each(|value| names.value(value));
    }
}
impl<P: PlanValueKind> sealed::Tree for crate::parser::SavedProviderConfigs<P> {
    fn visit(&self, names: &mut FunctionNames) {
        for config in self.as_provider_configs() {
            let crate::parser::ProviderConfig {
                name: _,
                attributes,
                default_tags,
                source: _,
                version: _,
                revision: _,
                unresolved_attributes,
                binding: _,
            } = config;
            attributes
                .values()
                .chain(default_tags.values())
                .chain(unresolved_attributes.values())
                .for_each(|value| names.value(value));
        }
    }
}
impl sealed::Tree for crate::parser::ExpandedBackendConfig {
    fn visit(&self, names: &mut FunctionNames) {
        let Self {
            backend_type: _,
            attributes,
            attribute_spans: _,
        } = self;
        attributes.values().for_each(|value| names.value(value));
    }
}
impl sealed::Tree for crate::resource::Current {
    fn visit(&self, names: &mut FunctionNames) {
        match self {
            Self::Absent(state) => names.tree(state.state()),
            Self::Present(state) => names.tree(state.state()),
            Self::MissingIdentifier(state) => names.tree(state.state()),
            Self::CreateInFlight(_pending) => {}
            Self::CreateOutcomeUnknown(_unknown) => {}
        }
    }
}
impl sealed::Tree for crate::data_source_value::DataSourceValue {
    fn visit(&self, names: &mut FunctionNames) {
        match self {
            Self::Read(state) => names.tree(state),
            Self::DeferredToApply(_inputs) => {}
            Self::BlockedByPlanError => {}
        }
    }
}
impl<T: DocumentTree> sealed::Tree for Box<T> {
    fn visit(&self, names: &mut FunctionNames) {
        names.tree(self.as_ref());
    }
}

macro_rules! effect_tree {
    ($node:ident, $this:expr, $names:expr) => {
        match $this {
            $node::Read { resource } => resource.as_inner().visit($names),
            $node::Create(resource) => resource.as_resource().visit($names),
            $node::ResumeCreate {
                resource,
                started_at: _started_at,
            } => resource.as_resource().visit($names),
            $node::Update {
                from,
                identifier: _identifier,
                to,
                changed_attributes: _changed_attributes,
            } => {
                from.visit($names);
                to.as_resource().visit($names);
            }
            $node::Import {
                id: _id,
                identifier,
            } => $names.value(identifier),
            $node::ReleaseListKeys {
                id: _id,
                attribute: _attribute,
                key_field: _key_field,
                keys,
            } => {
                for key in keys.iter() {
                    $names.value(key.retained_value());
                }
            }
            $node::DeferredCreate(payload) => payload.template.visit($names),
            $node::DeferredReplace(payload) => {
                payload.template.visit($names);
                for old in payload.deletes.iter() {
                    old.previous_attributes.visit($names);
                }
            }
            $node::Delete {
                id: _id,
                identifier: _identifier,
                generation: _generation,
                directives: _directives,
                binding: _binding,
                dependencies: _dependencies,
                explicit_dependencies: _explicit_dependencies,
                blocked_by_updates: _blocked_by_updates,
            } => {}
            $node::Remove { id: _id } => {}
            $node::Move {
                from: _from,
                to: _to,
                origin: _origin,
            } => {}
            $node::Wait {
                id: _id,
                target_id: _target_id,
                until: _until,
                until_surface: _until_surface,
                timeout: _timeout,
                interval: _interval,
                explicit_dependencies: _explicit_dependencies,
            } => {}
        }
    };
}
impl<S, P: PlanValueKind> sealed::Tree for crate::effect::Effect<S, P> {
    fn visit(&self, names: &mut FunctionNames) {
        use crate::effect::Effect as Node;
        effect_tree!(Node, self, names);
    }
}
impl sealed::Tree for crate::effect::UnresolvedSavedEffect {
    fn visit(&self, names: &mut FunctionNames) {
        use crate::effect::UnresolvedSavedEffect as Node;
        effect_tree!(Node, self, names);
    }
}

impl sealed::Tree for crate::replacement::ReplacementInputs {
    fn visit(&self, names: &mut FunctionNames) {
        let Self {
            old_instances,
            prepared_names: _prepared_names,
        } = self;
        for old in old_instances {
            let crate::replacement::OldInstanceSnapshot {
                instance: _instance,
                attributes,
            } = old;
            attributes.visit(names);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::deps::UnresolvedResource;
    use crate::resource::{Resource, SavedModuleSource};

    fn call(name: &str, file: &str) -> Value {
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse_file(
            &format!("fn {name}(x: test.Name): String {{ x }}"),
            std::path::Path::new(file),
            &context,
        )
        .unwrap();
        let value: crate::resource::AuthoredValue =
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: std::sync::Arc::new(parsed.user_functions[name].clone()),
                functions: std::sync::Arc::new(parsed.user_functions),
                args: vec![crate::resource::FunctionArgument::from_value(
                    crate::resource::AuthoredValue::resource_ref("src", "emitted", vec![]),
                )],
                source_line: 1.into(),
            });
        value.into_expanded().unwrap().resolve_root_for_test()
    }
    fn refusal(values: Vec<Value>) -> String {
        crate::value::with_saved_document(|conversion| conversion.snapshot(values))
            .unwrap_err()
            .to_string()
    }
    #[test]
    fn equal_function_labels_do_not_merge_distinct_declarations() {
        assert_eq!(
            refusal(vec![
                call("same", "one.crn"),
                call("same", "two.crn"),
                call("same", "one.crn")
            ]),
            "the plan contains unevaluated calls to the user-defined functions 'same' (one.crn:1), 'same' (two.crn:1). A saved plan cannot carry these calls."
        );
    }
    #[test]
    fn refusal_function_inventory_is_bounded_and_sorted() {
        let values = (0..10)
            .rev()
            .map(|index| call(&format!("f{index:02}"), &format!("f{index:02}.crn")))
            .collect();
        assert_eq!(
            refusal(values),
            "the plan contains unevaluated calls to the user-defined functions 'f00', 'f01', 'f02', 'f03', 'f04', 'f05', 'f06', 'f07' (and 2 more). A saved plan cannot carry these calls."
        );
    }

    #[test]
    fn authored_resource_keeps_references_and_serialized_bytes() {
        let authored: Resource = Resource::new("test.Sink", "sink")
            .with_attribute(
                "value",
                Value::root_resource_ref("src", "emitted", Vec::new()),
            )
            .with_attribute(
                "settings",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "nested".into(),
                    Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::Int(7)),
                        Value::Concrete(ConcreteValue::Bool(true)),
                    ])),
                )]))),
            );
        let source = UnresolvedResource::from_pre_resolve(authored);
        let evaluated: Resource = Resource::new("test.Sink", "sink").with_attribute(
            "value",
            Value::Concrete(ConcreteValue::String("evaluated".into())),
        );
        let expected = source.as_resource().to_snapshot();
        let saved = crate::value::with_saved_document(|conversion| {
            conversion.authored_resource(&source, &evaluated)
        })
        .unwrap();
        assert_eq!(saved.as_inner(), &expected);
        let bytes = serde_json::to_vec(&saved).unwrap();
        assert_eq!(bytes, serde_json::to_vec(&expected).unwrap());
        let decoded: SavedAuthored<
            Resource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>,
        > = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
    }

    #[test]
    fn authored_resource_admission_precedes_evaluated_replacement() {
        let source: Resource = Resource::new("test.Sink", "sink").with_attribute(
            "value",
            crate::value::retained_call_snapshot_tests::retained_call(),
        );
        let source = UnresolvedResource::from_pre_resolve(source);
        let evaluated: Resource = Resource::new("test.Sink", "sink").with_attribute(
            "value",
            Value::Concrete(ConcreteValue::String("evaluated".into())),
        );
        assert!(matches!(
            crate::value::with_saved_document(|conversion| {
                conversion.authored_resource(&source, &evaluated)
            }),
            Err(SerializationError::RetainedUserFunctionCall(_))
        ));
    }
}
