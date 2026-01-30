//! Interpreter - Execute Effects using a Provider
//!
//! The Interpreter executes Effects contained in a Plan in order,
//! collecting the results. This is where side effects actually occur.

use crate::effect::Effect;
use crate::plan::Plan;
use crate::provider::{Provider, ProviderError, ProviderResult};
use crate::resource::State;

/// Result of executing each Effect
#[derive(Debug)]
pub enum EffectOutcome {
    /// Read succeeded
    Read { state: State },
    /// Create succeeded
    Created { state: State },
    /// Update succeeded
    Updated { state: State },
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

    /// Execute a Plan, interpreting all Effects and causing side effects
    pub async fn apply(&self, plan: &Plan) -> ApplyResult {
        let mut outcomes = Vec::new();
        let mut success_count = 0;
        let mut failure_count = 0;

        for effect in plan.effects() {
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

    /// Execute a single Effect
    async fn execute_effect(&self, effect: &Effect) -> ProviderResult<EffectOutcome> {
        if self.config.dry_run {
            return Ok(EffectOutcome::Skipped {
                reason: "dry-run mode".to_string(),
            });
        }

        match effect {
            Effect::Read { resource } => {
                let state = self.provider.read(&resource.id).await?;
                Ok(EffectOutcome::Read { state })
            }
            Effect::Create(resource) => {
                let state = self.provider.create(resource).await?;
                Ok(EffectOutcome::Created { state })
            }
            Effect::Update { id, from, to } => {
                let state = self.provider.update(id, from, to).await?;
                Ok(EffectOutcome::Updated { state })
            }
            Effect::Delete(id) => {
                self.provider.delete(id).await?;
                Ok(EffectOutcome::Deleted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::BoxFuture;
    use crate::resource::{Resource, ResourceId};

    struct TestProvider;

    impl Provider for TestProvider {
        fn name(&self) -> &'static str {
            "test"
        }

        fn resource_types(&self) -> Vec<Box<dyn crate::provider::ResourceType>> {
            vec![]
        }

        fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::not_found(id)) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let state = State::existing(resource.id.clone(), resource.attributes.clone());
            Box::pin(async move { Ok(state) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _from: &State,
            to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let state = State::existing(id.clone(), to.attributes.clone());
            Box::pin(async move { Ok(state) })
        }

        fn delete(&self, _id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
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
}
