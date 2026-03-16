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
                    // Rename failure is non-fatal: the old resource is already deleted,
                    // so the replace succeeded — just with the temporary name.
                    let state = if let Some(temp) = temporary_name
                        && temp.can_rename
                    {
                        // Rename is non-fatal: if identifier is missing or update fails,
                        // fall back to keeping the temporary name.
                        if let Ok(new_identifier) = Self::require_identifier(&state, "rename") {
                            let mut rename_to = to.clone();
                            rename_to.attributes.insert(
                                temp.attribute.clone(),
                                crate::resource::Value::String(temp.original_value.clone()),
                            );
                            self.provider
                                .update(id, &new_identifier, &state, &rename_to)
                                .await
                                .unwrap_or(state)
                        } else {
                            state
                        }
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

        fn resource_types(&self) -> Vec<Box<dyn crate::provider::ResourceType>> {
            vec![]
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

        fn resource_types(&self) -> Vec<Box<dyn crate::provider::ResourceType>> {
            vec![]
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
        });

        let result = interpreter.apply(&plan).await;
        assert!(result.is_success());

        // Verify order: create (with temp name) → delete (old) — no rename step
        let ops = ops.lock().unwrap();
        assert_eq!(*ops, vec!["create", "delete"]);
    }
}
