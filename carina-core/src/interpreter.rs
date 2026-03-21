//! Interpreter - Execute Effects using a Provider
//!
//! The Interpreter executes Effects contained in a Plan in order,
//! collecting the results. This is where side effects actually occur.

use std::collections::{HashMap, HashSet};

use crate::effect::Effect;
use crate::plan::Plan;
use crate::provider::{Provider, ProviderError, ProviderResult};
use crate::resource::{State, Value};

/// Result of executing each Effect
#[derive(Debug)]
pub enum EffectOutcome {
    /// Read succeeded
    Read { state: State },
    /// Create succeeded
    Created { state: State },
    /// Update succeeded
    Updated { state: State },
    /// Replace succeeded (delete then create)
    Replaced { state: State },
    /// Delete succeeded
    Deleted,
    /// Skipped (e.g., dry-run)
    Skipped { reason: String },
}

/// Result of executing the entire Plan
#[derive(Debug)]
pub struct ApplyResult {
    pub outcomes: Vec<Result<EffectOutcome, ProviderError>>,
    pub success_count: usize,
    pub failure_count: usize,
}

impl ApplyResult {
    pub fn is_success(&self) -> bool {
        self.failure_count == 0
    }
}

/// Interpreter configuration
#[derive(Debug, Clone, Default)]
pub struct InterpreterConfig {
    /// If true, skip actual side effects
    pub dry_run: bool,
    /// Continue on error
    pub continue_on_error: bool,
}

/// Interpreter that executes Effects using a Provider
pub struct Interpreter<P: Provider> {
    provider: P,
    config: InterpreterConfig,
}

impl<P: Provider> Interpreter<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            config: InterpreterConfig::default(),
        }
    }

    pub fn with_config(mut self, config: InterpreterConfig) -> Self {
        self.config = config;
        self
    }

    /// Execute a Plan, interpreting all Effects and causing side effects.
    ///
    /// When multiple Replace effects have dependency relationships (indicated by
    /// `_binding` and `_dependency_bindings` attributes), the interpreter decomposes
    /// them into phases to respect dependency order:
    ///
    /// 1. **CBD creates** (forward dependency order): parents first, then dependents
    /// 2. **All deletes** (reverse dependency order): dependents first, then parents
    /// 3. **Non-CBD creates** (forward dependency order): parents first, then dependents
    ///
    /// This ensures that dependent resources are deleted before parent resources,
    /// and parent resources are created before dependent resources.
    pub async fn apply(&self, plan: &Plan) -> ApplyResult {
        let effects = plan.effects();

        // Check if we have multiple Replace effects with dependency relationships
        if self.has_interdependent_replaces(effects) {
            self.apply_with_dependency_order(effects).await
        } else {
            self.apply_sequential(effects).await
        }
    }

    /// Check if the plan contains multiple Replace effects that depend on each other.
    fn has_interdependent_replaces(&self, effects: &[Effect]) -> bool {
        let replace_bindings = Self::collect_replace_bindings(effects);
        if replace_bindings.is_empty() {
            return false;
        }

        // Check if any Replace effect depends on another Replace effect's binding
        for effect in effects {
            if let Effect::Replace { from, .. } = effect {
                let dep_bindings = Self::extract_dependency_bindings(&from.attributes);
                for dep in &dep_bindings {
                    if replace_bindings.contains(dep) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Collect binding names from all Replace effects.
    fn collect_replace_bindings(effects: &[Effect]) -> HashSet<String> {
        let mut bindings = HashSet::new();
        for effect in effects {
            if let Effect::Replace { to, .. } = effect
                && let Some(Value::String(b)) = to.attributes.get("_binding")
            {
                bindings.insert(b.clone());
            }
        }
        bindings
    }

    /// Extract `_dependency_bindings` from attributes.
    fn extract_dependency_bindings(attrs: &HashMap<String, Value>) -> Vec<String> {
        match attrs.get("_dependency_bindings") {
            Some(Value::List(list)) => list
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        }
    }

    /// Execute effects sequentially (original behavior, no dependency reordering).
    async fn apply_sequential(&self, effects: &[Effect]) -> ApplyResult {
        let mut outcomes = Vec::new();
        let mut success_count = 0;
        let mut failure_count = 0;

        for effect in effects {
            let result = self.execute_effect(effect).await;

            match &result {
                Ok(_) => success_count += 1,
                Err(_) => {
                    failure_count += 1;
                    if !self.config.continue_on_error {
                        outcomes.push(result);
                        break;
                    }
                }
            }

            outcomes.push(result);
        }

        ApplyResult {
            outcomes,
            success_count,
            failure_count,
        }
    }

    /// Execute effects with dependency-aware ordering for Replace effects.
    ///
    /// Decomposes Replace effects into create and delete operations, then executes:
    /// 1. Non-Replace effects and CBD creates in forward dependency order
    /// 2. All deletes (from Replace) in reverse dependency order
    /// 3. Non-CBD creates in forward dependency order
    async fn apply_with_dependency_order(&self, effects: &[Effect]) -> ApplyResult {
        let mut outcomes = Vec::new();
        let mut success_count = 0;
        let mut failure_count = 0;

        // Build dependency graph among Replace effects
        let replace_bindings = Self::collect_replace_bindings(effects);

        // Topologically sort Replace effects by dependency order
        let sorted_indices = self.topological_sort_replaces(effects, &replace_bindings);

        // Phase 1: Execute non-Replace effects and CBD creates (forward dependency order)
        // First, non-Replace effects in original order
        for (idx, effect) in effects.iter().enumerate() {
            if !matches!(effect, Effect::Replace { .. }) {
                let result = self.execute_effect(effect).await;
                match &result {
                    Ok(_) => success_count += 1,
                    Err(_) => {
                        failure_count += 1;
                        if !self.config.continue_on_error {
                            outcomes.push(result);
                            return ApplyResult {
                                outcomes,
                                success_count,
                                failure_count,
                            };
                        }
                    }
                }
                outcomes.push(result);
            } else {
                // Placeholder for Replace effects - will be filled later
                let _ = idx;
            }
        }

        // CBD creates in forward dependency order (parents first)
        let mut cbd_create_states: HashMap<usize, State> = HashMap::new();
        for &idx in &sorted_indices {
            let effect = &effects[idx];
            if let Effect::Replace {
                to,
                lifecycle,
                cascading_updates,
                ..
            } = effect
                && lifecycle.create_before_destroy
            {
                let result = self.provider.create(to).await;
                match result {
                    Ok(state) => {
                        // Execute cascading updates
                        for cascade in cascading_updates {
                            let cascade_identifier =
                                Self::require_identifier(&cascade.from, "cascading update");
                            match cascade_identifier {
                                Ok(ident) => {
                                    let update_result = self
                                        .provider
                                        .update(&cascade.id, &ident, &cascade.from, &cascade.to)
                                        .await;
                                    if let Err(e) = update_result {
                                        failure_count += 1;
                                        if !self.config.continue_on_error {
                                            outcomes.push(Err(e));
                                            return ApplyResult {
                                                outcomes,
                                                success_count,
                                                failure_count,
                                            };
                                        }
                                        outcomes.push(Err(e));
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    failure_count += 1;
                                    if !self.config.continue_on_error {
                                        outcomes.push(Err(e));
                                        return ApplyResult {
                                            outcomes,
                                            success_count,
                                            failure_count,
                                        };
                                    }
                                    outcomes.push(Err(e));
                                    continue;
                                }
                            }
                        }
                        cbd_create_states.insert(idx, state);
                    }
                    Err(e) => {
                        failure_count += 1;
                        if !self.config.continue_on_error {
                            outcomes.push(Err(e));
                            return ApplyResult {
                                outcomes,
                                success_count,
                                failure_count,
                            };
                        }
                        outcomes.push(Err(e));
                    }
                }
            }
        }

        // Phase 2: All deletes in reverse dependency order (dependents first)
        for &idx in sorted_indices.iter().rev() {
            let effect = &effects[idx];
            if let Effect::Replace {
                id,
                from,
                lifecycle,
                ..
            } = effect
            {
                let identifier = Self::require_identifier(from, "delete (replace)");
                match identifier {
                    Ok(ident) => {
                        let result = self.provider.delete(id, &ident, lifecycle).await;
                        if let Err(e) = result {
                            failure_count += 1;
                            if !self.config.continue_on_error {
                                outcomes.push(Err(e));
                                return ApplyResult {
                                    outcomes,
                                    success_count,
                                    failure_count,
                                };
                            }
                            outcomes.push(Err(e));
                        }
                    }
                    Err(e) => {
                        failure_count += 1;
                        if !self.config.continue_on_error {
                            outcomes.push(Err(e));
                            return ApplyResult {
                                outcomes,
                                success_count,
                                failure_count,
                            };
                        }
                        outcomes.push(Err(e));
                    }
                }
            }
        }

        // Phase 3: Non-CBD creates in forward dependency order (parents first)
        for &idx in &sorted_indices {
            let effect = &effects[idx];
            if let Effect::Replace {
                id,
                to,
                lifecycle,
                temporary_name,
                ..
            } = effect
            {
                if lifecycle.create_before_destroy {
                    // CBD: already created in phase 1, handle rename if needed
                    if let Some(state) = cbd_create_states.remove(&idx) {
                        let final_state = if let Some(temp) = temporary_name
                            && temp.can_rename
                        {
                            let new_identifier = Self::require_identifier(&state, "rename");
                            match new_identifier {
                                Ok(ident) => {
                                    let mut rename_to = to.clone();
                                    rename_to.attributes.insert(
                                        temp.attribute.clone(),
                                        Value::String(temp.original_value.clone()),
                                    );
                                    match self.provider.update(id, &ident, &state, &rename_to).await
                                    {
                                        Ok(s) => s,
                                        Err(e) => {
                                            failure_count += 1;
                                            outcomes.push(Err(e));
                                            if !self.config.continue_on_error {
                                                return ApplyResult {
                                                    outcomes,
                                                    success_count,
                                                    failure_count,
                                                };
                                            }
                                            continue;
                                        }
                                    }
                                }
                                Err(e) => {
                                    failure_count += 1;
                                    outcomes.push(Err(e));
                                    if !self.config.continue_on_error {
                                        return ApplyResult {
                                            outcomes,
                                            success_count,
                                            failure_count,
                                        };
                                    }
                                    continue;
                                }
                            }
                        } else {
                            state
                        };
                        success_count += 1;
                        outcomes.push(Ok(EffectOutcome::Replaced { state: final_state }));
                    }
                } else {
                    // Non-CBD: create the new resource now (after delete)
                    let result = self.provider.create(to).await;
                    match result {
                        Ok(state) => {
                            success_count += 1;
                            outcomes.push(Ok(EffectOutcome::Replaced { state }));
                        }
                        Err(e) => {
                            failure_count += 1;
                            if !self.config.continue_on_error {
                                outcomes.push(Err(e));
                                return ApplyResult {
                                    outcomes,
                                    success_count,
                                    failure_count,
                                };
                            }
                            outcomes.push(Err(e));
                        }
                    }
                }
            }
        }

        ApplyResult {
            outcomes,
            success_count,
            failure_count,
        }
    }

    /// Topologically sort Replace effects by dependency order.
    /// Returns indices in forward dependency order (parents before dependents).
    fn topological_sort_replaces(
        &self,
        effects: &[Effect],
        replace_bindings: &HashSet<String>,
    ) -> Vec<usize> {
        // Map binding name -> effect index
        let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
        let mut replace_indices: Vec<usize> = Vec::new();

        for (idx, effect) in effects.iter().enumerate() {
            if let Effect::Replace { to, .. } = effect {
                replace_indices.push(idx);
                if let Some(Value::String(b)) = to.attributes.get("_binding") {
                    binding_to_idx.insert(b.clone(), idx);
                }
            }
        }

        // Build adjacency: for each replace effect, find which other replace effects it depends on
        let mut deps: HashMap<usize, Vec<usize>> = HashMap::new();
        for &idx in &replace_indices {
            let effect = &effects[idx];
            if let Effect::Replace { from, to, .. } = effect {
                // Check both from and to for dependency bindings
                let dep_bindings_from = Self::extract_dependency_bindings(&from.attributes);
                let dep_bindings_to = Self::extract_dependency_bindings(&to.attributes);
                let mut all_deps = dep_bindings_from;
                for d in dep_bindings_to {
                    if !all_deps.contains(&d) {
                        all_deps.push(d);
                    }
                }

                let dep_indices: Vec<usize> = all_deps
                    .iter()
                    .filter(|b| replace_bindings.contains(*b))
                    .filter_map(|b| binding_to_idx.get(b))
                    .copied()
                    .collect();
                deps.insert(idx, dep_indices);
            }
        }

        // Kahn's algorithm for topological sort
        let mut in_degree: HashMap<usize, usize> = HashMap::new();
        for &idx in &replace_indices {
            in_degree.insert(idx, 0);
        }
        // in_degree counts how many dependencies each node has
        for (&idx, dep_list) in &deps {
            *in_degree.entry(idx).or_insert(0) += dep_list.len();
        }

        let mut queue: Vec<usize> = replace_indices
            .iter()
            .filter(|idx| *in_degree.get(idx).unwrap_or(&0) == 0)
            .copied()
            .collect();
        queue.sort(); // Deterministic order for nodes with no dependencies

        let mut sorted = Vec::new();
        while let Some(node) = queue.pop() {
            sorted.push(node);
            // Find nodes that depend on this node and reduce their in-degree
            for (&idx, dep_list) in &deps {
                if dep_list.contains(&node) {
                    let deg = in_degree.get_mut(&idx).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(idx);
                        queue.sort();
                    }
                }
            }
        }

        // If there are nodes not in sorted (cycle), append them in original order
        for &idx in &replace_indices {
            if !sorted.contains(&idx) {
                sorted.push(idx);
            }
        }

        sorted
    }

    /// Extract identifier from state, returning an error if missing.
    fn require_identifier(state: &State, operation: &str) -> ProviderResult<String> {
        state
            .identifier
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ProviderError::new(format!(
                    "missing resource identifier for {} operation",
                    operation
                ))
                .for_resource(state.id.clone())
            })
    }

    /// Execute a single Effect
    async fn execute_effect(&self, effect: &Effect) -> ProviderResult<EffectOutcome> {
        if self.config.dry_run {
            return Ok(EffectOutcome::Skipped {
                reason: "dry-run mode".to_string(),
            });
        }

        match effect {
            Effect::Read { resource } => {
                // Read without identifier (fall back to name-based lookup)
                let state = self.provider.read(&resource.id, None).await?;
                Ok(EffectOutcome::Read { state })
            }
            Effect::Create(resource) => {
                let state = self.provider.create(resource).await?;
                Ok(EffectOutcome::Created { state })
            }
            Effect::Update { id, from, to, .. } => {
                let identifier = Self::require_identifier(from, "update")?;
                let state = self.provider.update(id, &identifier, from, to).await?;
                Ok(EffectOutcome::Updated { state })
            }
            Effect::Replace {
                id,
                from,
                to,
                lifecycle,
                cascading_updates,
                temporary_name,
                ..
            } => {
                if lifecycle.create_before_destroy {
                    // Create the new resource first (possibly with a temporary name)
                    let state = self.provider.create(to).await?;
                    // Execute cascading updates for dependent resources
                    for cascade in cascading_updates {
                        let cascade_identifier =
                            Self::require_identifier(&cascade.from, "cascading update")?;
                        self.provider
                            .update(&cascade.id, &cascade_identifier, &cascade.from, &cascade.to)
                            .await?;
                    }
                    // Then delete the old resource
                    let identifier = Self::require_identifier(from, "delete (replace)")?;
                    self.provider.delete(id, &identifier, lifecycle).await?;
                    // If a temporary name was used and the name is updatable,
                    // rename the new resource back to the desired name.
                    let state = if let Some(temp) = temporary_name
                        && temp.can_rename
                    {
                        let new_identifier = Self::require_identifier(&state, "rename")?;
                        let mut rename_to = to.clone();
                        rename_to.attributes.insert(
                            temp.attribute.clone(),
                            crate::resource::Value::String(temp.original_value.clone()),
                        );
                        self.provider
                            .update(id, &new_identifier, &state, &rename_to)
                            .await?
                    } else {
                        state
                    };
                    Ok(EffectOutcome::Replaced { state })
                } else {
                    // Delete the existing resource first
                    let identifier = Self::require_identifier(from, "delete (replace)")?;
                    self.provider.delete(id, &identifier, lifecycle).await?;
                    // Then create the new resource
                    let state = self.provider.create(to).await?;
                    Ok(EffectOutcome::Replaced { state })
                }
            }
            Effect::Delete {
                id,
                identifier,
                lifecycle,
                ..
            } => {
                self.provider.delete(id, identifier, lifecycle).await?;
                Ok(EffectOutcome::Deleted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BoxFuture;
    use crate::resource::{LifecycleConfig, Resource, ResourceId};

    struct TestProvider;

    impl Provider for TestProvider {
        fn name(&self) -> &'static str {
            "test"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let state = State::existing(resource.id.clone(), resource.attributes.clone())
                .with_identifier("test-id");
            Box::pin(async move { Ok(state) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let state = State::existing(id.clone(), to.attributes.clone());
            Box::pin(async move { Ok(state) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn apply_empty_plan() {
        let interpreter = Interpreter::new(TestProvider);
        let plan = Plan::new();
        let result = interpreter.apply(&plan).await;

        assert!(result.is_success());
        assert_eq!(result.success_count, 0);
    }

    #[tokio::test]
    async fn apply_create_effect() {
        let interpreter = Interpreter::new(TestProvider);
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("test", "example")));

        let result = interpreter.apply(&plan).await;

        assert!(result.is_success());
        assert_eq!(result.success_count, 1);
    }

    #[tokio::test]
    async fn apply_replace_effect() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let interpreter = Interpreter::new(TestProvider);
        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("test", "example"),
            from: Box::new(
                State::existing(
                    ResourceId::new("test", "example"),
                    HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
                )
                .with_identifier("test-id"),
            ),
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["key".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;

        assert!(result.is_success());
        assert_eq!(result.success_count, 1);
        assert!(matches!(
            result.outcomes[0],
            Ok(EffectOutcome::Replaced { .. })
        ));
    }

    #[tokio::test]
    async fn dry_run_skips_effects() {
        let config = InterpreterConfig {
            dry_run: true,
            ..Default::default()
        };
        let interpreter = Interpreter::new(TestProvider).with_config(config);
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("test", "example")));

        let result = interpreter.apply(&plan).await;

        assert!(result.is_success());
        assert!(matches!(
            result.outcomes[0],
            Ok(EffectOutcome::Skipped { .. })
        ));
    }

    /// Provider that tracks the order of operations
    struct OrderTrackingProvider {
        ops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Provider for OrderTrackingProvider {
        fn name(&self) -> &'static str {
            "order_tracking"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            self.ops.lock().unwrap().push("create".to_string());
            let state = State::existing(resource.id.clone(), resource.attributes.clone())
                .with_identifier("test-id");
            Box::pin(async move { Ok(state) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.ops.lock().unwrap().push("update".to_string());
            let state = State::existing(id.clone(), to.attributes.clone());
            Box::pin(async move { Ok(state) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            self.ops.lock().unwrap().push("delete".to_string());
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn replace_default_order_delete_then_create() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = OrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("test", "example"),
            from: Box::new(
                State::existing(
                    ResourceId::new("test", "example"),
                    HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
                )
                .with_identifier("test-id"),
            ),
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["key".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["delete", "create"]);
    }

    #[tokio::test]
    async fn replace_create_before_destroy_order() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = OrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("test", "example"),
            from: Box::new(
                State::existing(
                    ResourceId::new("test", "example"),
                    HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
                )
                .with_identifier("test-id"),
            ),
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["key".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "delete"]);
    }

    #[tokio::test]
    async fn replace_create_before_destroy_with_cascading_updates() {
        use crate::effect::CascadingUpdate;
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = OrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "my-vpc"),
            from: Box::new(
                State::existing(
                    ResourceId::new("ec2.vpc", "my-vpc"),
                    HashMap::from([(
                        "cidr_block".to_string(),
                        Value::String("10.0.0.0/16".to_string()),
                    )]),
                )
                .with_identifier("vpc-old"),
            ),
            to: Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string())),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![CascadingUpdate {
                id: ResourceId::new("ec2.subnet", "my-subnet"),
                from: Box::new(
                    State::existing(
                        ResourceId::new("ec2.subnet", "my-subnet"),
                        HashMap::from([(
                            "vpc_id".to_string(),
                            Value::String("vpc-old".to_string()),
                        )]),
                    )
                    .with_identifier("subnet-123"),
                ),
                to: Resource::new("ec2.subnet", "my-subnet")
                    .with_attribute("vpc_id", Value::String("vpc-new".to_string())),
            }],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        // Verify order: create (new VPC) → update (subnet cascade) → delete (old VPC)
        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "update", "delete"]);
    }

    #[tokio::test]
    async fn replace_create_before_destroy_with_temporary_name_and_rename() {
        use crate::effect::TemporaryName;
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = OrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("s3.bucket", "my-bucket"),
            from: Box::new(
                State::existing(
                    ResourceId::new("s3.bucket", "my-bucket"),
                    HashMap::from([
                        (
                            "bucket_name".to_string(),
                            Value::String("my-bucket".to_string()),
                        ),
                        ("object_lock_enabled".to_string(), Value::Bool(false)),
                    ]),
                )
                .with_identifier("my-bucket"),
            ),
            to: Resource::new("s3.bucket", "my-bucket")
                .with_attribute(
                    "bucket_name",
                    Value::String("my-bucket-abc12345".to_string()),
                )
                .with_attribute("object_lock_enabled", Value::Bool(true)),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["object_lock_enabled".to_string()],
            cascading_updates: vec![],
            temporary_name: Some(TemporaryName {
                attribute: "bucket_name".to_string(),
                original_value: "my-bucket".to_string(),
                temporary_value: "my-bucket-abc12345".to_string(),
                can_rename: true, // name is updatable
            }),
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        // Verify order: create (with temp name) → delete (old) → update (rename back)
        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "delete", "update"]);
    }

    #[tokio::test]
    async fn update_missing_identifier_returns_error() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let interpreter = Interpreter::new(TestProvider);
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            id: ResourceId::new("test", "example"),
            from: Box::new(State::existing(
                ResourceId::new("test", "example"),
                HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
            )),
            // No identifier set on `from`
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            changed_attributes: vec!["key".to_string()],
        });

        let result = interpreter.apply(&plan).await;
        assert_eq!(result.failure_count, 1);
        let err = result.outcomes[0].as_ref().unwrap_err();
        assert!(
            err.message.contains("identifier"),
            "expected error about missing identifier, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn replace_missing_identifier_returns_error() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let interpreter = Interpreter::new(TestProvider);
        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("test", "example"),
            from: Box::new(State::existing(
                ResourceId::new("test", "example"),
                HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
            )),
            // No identifier set on `from`
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["key".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert_eq!(result.failure_count, 1);
        let err = result.outcomes[0].as_ref().unwrap_err();
        assert!(
            err.message.contains("identifier"),
            "expected error about missing identifier, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn replace_create_before_destroy_missing_identifier_returns_error() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let interpreter = Interpreter::new(TestProvider);
        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("test", "example"),
            from: Box::new(State::existing(
                ResourceId::new("test", "example"),
                HashMap::from([("key".to_string(), Value::String("old".to_string()))]),
            )),
            // No identifier set on `from`
            to: Resource::new("test", "example")
                .with_attribute("key", Value::String("new".to_string())),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["key".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert_eq!(result.failure_count, 1);
        let err = result.outcomes[0].as_ref().unwrap_err();
        assert!(
            err.message.contains("identifier"),
            "expected error about missing identifier, got: {}",
            err.message
        );
    }

    /// Provider that fails on the rename (update) step after delete in create-before-destroy
    struct RenameFailProvider {
        ops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Provider for RenameFailProvider {
        fn name(&self) -> &'static str {
            "rename_fail"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            self.ops.lock().unwrap().push("create".to_string());
            let state = State::existing(resource.id.clone(), resource.attributes.clone())
                .with_identifier("new-id");
            Box::pin(async move { Ok(state) })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _from: &State,
            _to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            self.ops.lock().unwrap().push("update".to_string());
            Box::pin(async move { Err(ProviderError::new("rename failed: service unavailable")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            self.ops.lock().unwrap().push("delete".to_string());
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn replace_create_before_destroy_rename_failure_returns_error() {
        use crate::effect::TemporaryName;
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = RenameFailProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("s3.bucket", "my-bucket"),
            from: Box::new(
                State::existing(
                    ResourceId::new("s3.bucket", "my-bucket"),
                    HashMap::from([
                        (
                            "bucket_name".to_string(),
                            Value::String("my-bucket".to_string()),
                        ),
                        ("object_lock_enabled".to_string(), Value::Bool(false)),
                    ]),
                )
                .with_identifier("my-bucket"),
            ),
            to: Resource::new("s3.bucket", "my-bucket")
                .with_attribute(
                    "bucket_name",
                    Value::String("my-bucket-abc12345".to_string()),
                )
                .with_attribute("object_lock_enabled", Value::Bool(true)),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["object_lock_enabled".to_string()],
            cascading_updates: vec![],
            temporary_name: Some(TemporaryName {
                attribute: "bucket_name".to_string(),
                original_value: "my-bucket".to_string(),
                temporary_value: "my-bucket-abc12345".to_string(),
                can_rename: true,
            }),
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;

        // Rename failure should be reported as an error
        assert_eq!(result.failure_count, 1);
        let err = result.outcomes[0].as_ref().unwrap_err();
        assert!(
            err.message.contains("rename failed"),
            "expected rename failure error, got: {}",
            err.message
        );

        // Operations should still have been: create, delete, update (rename attempt)
        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "delete", "update"]);
    }

    #[tokio::test]
    async fn replace_create_before_destroy_with_temporary_name_no_rename() {
        use crate::effect::TemporaryName;
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = OrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();
        plan.add(Effect::Replace {
            id: ResourceId::new("s3.bucket", "my-bucket"),
            from: Box::new(
                State::existing(
                    ResourceId::new("s3.bucket", "my-bucket"),
                    HashMap::from([
                        (
                            "bucket_name".to_string(),
                            Value::String("my-bucket".to_string()),
                        ),
                        ("object_lock_enabled".to_string(), Value::Bool(false)),
                    ]),
                )
                .with_identifier("my-bucket"),
            ),
            to: Resource::new("s3.bucket", "my-bucket")
                .with_attribute(
                    "bucket_name",
                    Value::String("my-bucket-abc12345".to_string()),
                )
                .with_attribute("object_lock_enabled", Value::Bool(true)),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["object_lock_enabled".to_string()],
            cascading_updates: vec![],
            temporary_name: Some(TemporaryName {
                attribute: "bucket_name".to_string(),
                original_value: "my-bucket".to_string(),
                temporary_value: "my-bucket-abc12345".to_string(),
                can_rename: false, // name is create-only, cannot rename
            }),
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        // Verify order: create (with temp name) → delete (old) — no rename step
        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "delete"]);
    }

    /// Provider that tracks operations with resource type info (e.g., "create:ec2.vpc")
    struct DetailedOrderTrackingProvider {
        ops: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Provider for DetailedOrderTrackingProvider {
        fn name(&self) -> &'static str {
            "detailed_order_tracking"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let op = format!("create:{}", resource.id.resource_type);
            self.ops.lock().unwrap().push(op);
            let state = State::existing(resource.id.clone(), resource.attributes.clone())
                .with_identifier(format!("{}-new-id", resource.id.resource_type));
            Box::pin(async move { Ok(state) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let op = format!("update:{}", id.resource_type);
            self.ops.lock().unwrap().push(op);
            let state = State::existing(id.clone(), to.attributes.clone());
            Box::pin(async move { Ok(state) })
        }

        fn delete(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            let op = format!("delete:{}", id.resource_type);
            self.ops.lock().unwrap().push(op);
            Box::pin(async { Ok(()) })
        }
    }

    /// Test that when VPC (CBD) and Subnet (non-CBD) are both replaced,
    /// the interpreter respects dependency order:
    /// - VPC create (new) should happen before subnet create (new)
    /// - Subnet delete (old) should happen before VPC delete (old)
    ///
    /// Currently fails because the interpreter executes effects in plan order,
    /// completing VPC Replace (create new → delete old) before starting Subnet Replace.
    /// This means VPC delete happens while subnet still references the old VPC.
    #[tokio::test]
    async fn replace_multiple_effects_respects_dependency_order() {
        use crate::resource::Value;
        use std::collections::HashMap;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = DetailedOrderTrackingProvider { ops: ops.clone() };
        let interpreter = Interpreter::new(provider);

        let mut plan = Plan::new();

        // Effect 1: VPC Replace with create_before_destroy (added to plan first)
        // VPC cidr_block changed → must be replaced
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.vpc", "my-vpc"),
            from: Box::new(
                State::existing(
                    ResourceId::new("ec2.vpc", "my-vpc"),
                    HashMap::from([
                        (
                            "cidr_block".to_string(),
                            Value::String("10.0.0.0/16".to_string()),
                        ),
                        ("_binding".to_string(), Value::String("vpc".to_string())),
                    ]),
                )
                .with_identifier("vpc-old-id"),
            ),
            to: Resource::new("ec2.vpc", "my-vpc")
                .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()))
                .with_attribute("_binding", Value::String("vpc".to_string())),
            lifecycle: LifecycleConfig {
                force_delete: false,
                create_before_destroy: true,
            },
            changed_create_only: vec!["cidr_block".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        // Effect 2: Subnet Replace (non-CBD, default delete-then-create)
        // Subnet's vpc_id is create-only and changed because VPC was replaced
        plan.add(Effect::Replace {
            id: ResourceId::new("ec2.subnet", "my-subnet"),
            from: Box::new(
                State::existing(
                    ResourceId::new("ec2.subnet", "my-subnet"),
                    HashMap::from([
                        (
                            "vpc_id".to_string(),
                            Value::String("vpc-old-id".to_string()),
                        ),
                        (
                            "cidr_block".to_string(),
                            Value::String("10.0.1.0/24".to_string()),
                        ),
                        ("_binding".to_string(), Value::String("subnet".to_string())),
                        (
                            "_dependency_bindings".to_string(),
                            Value::List(vec![Value::String("vpc".to_string())]),
                        ),
                    ]),
                )
                .with_identifier("subnet-old-id"),
            ),
            to: Resource::new("ec2.subnet", "my-subnet")
                .with_attribute("vpc_id", Value::String("vpc-new-id".to_string()))
                .with_attribute("cidr_block", Value::String("10.1.1.0/24".to_string()))
                .with_attribute("_binding", Value::String("subnet".to_string()))
                .with_attribute(
                    "_dependency_bindings",
                    Value::List(vec![Value::String("vpc".to_string())]),
                ),
            lifecycle: LifecycleConfig::default(),
            changed_create_only: vec!["vpc_id".to_string()],
            cascading_updates: vec![],
            temporary_name: None,
            cascade_ref_hints: vec![],
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        let ops = ops.lock().unwrap();

        // The correct execution order should be:
        // 1. create:ec2.vpc    (VPC CBD: create new VPC first)
        // 2. delete:ec2.subnet (Subnet: delete old subnet that references old VPC)
        // 3. delete:ec2.vpc    (VPC CBD: delete old VPC, now safe since subnet is gone)
        // 4. create:ec2.subnet (Subnet: create new subnet in new VPC)
        //
        // Key invariants:
        // - Subnet delete MUST happen BEFORE VPC delete
        //   (old subnet references old VPC, can't delete VPC while subnet exists)
        // - VPC create MUST happen BEFORE Subnet create
        //   (new subnet needs new VPC to exist)

        let vpc_delete_idx = ops
            .iter()
            .position(|op| op == "delete:ec2.vpc")
            .expect("should have VPC delete");
        let subnet_delete_idx = ops
            .iter()
            .position(|op| op == "delete:ec2.subnet")
            .expect("should have subnet delete");
        let vpc_create_idx = ops
            .iter()
            .position(|op| op == "create:ec2.vpc")
            .expect("should have VPC create");
        let subnet_create_idx = ops
            .iter()
            .position(|op| op == "create:ec2.subnet")
            .expect("should have subnet create");

        assert!(
            subnet_delete_idx < vpc_delete_idx,
            "subnet delete (at {}) must happen before VPC delete (at {}), \
             but got ops: {:?}",
            subnet_delete_idx,
            vpc_delete_idx,
            *ops
        );

        assert!(
            vpc_create_idx < subnet_create_idx,
            "VPC create (at {}) must happen before subnet create (at {}), \
             but got ops: {:?}",
            vpc_create_idx,
            subnet_create_idx,
            *ops
        );
    }
}
