//! Polling loop for `Effect::Wait`.
//!
//! The wait construct (carina#2825) is dispatched by carina-core's
//! executor — not by the provider — so providers (including WASM
//! plugins) need no contract change beyond the existing
//! [`Provider::read`] trait method.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` §Executor logic.

use std::time::{Duration, Instant};

use crate::provider::{Provider, ProviderError, ProviderResult, ReadRequest};
use crate::resource::{ResourceId, State};
use crate::wait::predicate::WaitPredicate;

/// Run the wait polling loop:
///
/// 1. `read()` the target's current state.
/// 2. If the read returns `not_found`, fail immediately with
///    [`ProviderError::NotFound`] — mid-poll target deletion is a
///    real divergence the user should see right away.
/// 3. Evaluate `until` against the returned attribute map.
/// 4. If true, return the captured state.
/// 5. Otherwise, if the elapsed time has passed `timeout`, return
///    [`ProviderError::Timeout`] whose message includes the unmet
///    predicate, the last observed attribute snapshot, and the
///    elapsed time.
/// 6. Otherwise, sleep for `interval` and repeat.
pub async fn execute_wait_effect(
    provider: &dyn Provider,
    target_id: &ResourceId,
    target_identifier: Option<&str>,
    until: &WaitPredicate,
    timeout: Duration,
    interval: Duration,
) -> ProviderResult<State> {
    let start = Instant::now();
    loop {
        let state = provider
            .read(target_id, target_identifier, ReadRequest)
            .await?;
        if !state.exists {
            return Err(ProviderError::not_found(format!(
                "wait target {} not found (deleted out-of-band?)",
                target_id
            ))
            .for_resource(target_id.clone()));
        }
        if until.evaluate(&state.attributes) {
            return Ok(state);
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(ProviderError::timeout(format!(
                "wait on {} timed out after {:?}; predicate `{:?}` never became true (last observed attributes: {:?})",
                target_id, timeout, until, state.attributes
            ))
            .for_resource(target_id.clone()));
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{BoxFuture, CreateRequest, DeleteRequest, ReadRequest, UpdateRequest};
    use crate::resource::{ConcreteValue, DataSource, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal test provider: returns a queue of pre-canned read
    /// responses in order, then repeats the last one. Counts every
    /// `read` call so tests can assert polling behaviour.
    struct ReadSequenceProvider {
        responses: Mutex<Vec<State>>,
        reads: Mutex<usize>,
    }

    impl ReadSequenceProvider {
        fn new(responses: Vec<State>) -> Self {
            Self {
                responses: Mutex::new(responses),
                reads: Mutex::new(0),
            }
        }

        fn read_count(&self) -> usize {
            *self.reads.lock().unwrap()
        }
    }

    impl Provider for ReadSequenceProvider {
        fn name(&self) -> &str {
            "mock-wait"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async move {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                let mut responses = self.responses.lock().unwrap();
                if responses.len() > 1 {
                    Ok(responses.remove(0))
                } else if let Some(last) = responses.last() {
                    Ok(last.clone())
                } else {
                    Err(ProviderError::api_error("no canned response"))
                }
            })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn update(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async move { Ok(()) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    fn state_with_status(status: &str) -> State {
        let mut attrs = HashMap::new();
        attrs.insert(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String(status.to_string())),
        );
        State::existing(ResourceId::new("acm.Certificate", "cert"), attrs)
    }

    fn equals_status(value: &str) -> WaitPredicate {
        WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String(value.to_string())),
        }
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_until_already_true() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("ISSUED")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(10),
        )
        .await;
        let state = result.expect("wait should succeed");
        assert_eq!(
            state.attributes.get("status"),
            Some(&Value::Concrete(ConcreteValue::String(
                "ISSUED".to_string()
            )))
        );
        assert_eq!(provider.read_count(), 1);
    }

    #[tokio::test]
    async fn wait_polls_until_predicate_becomes_true() {
        let provider = ReadSequenceProvider::new(vec![
            state_with_status("PENDING_VALIDATION"),
            state_with_status("PENDING_VALIDATION"),
            state_with_status("ISSUED"),
        ]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(1),
        )
        .await;
        assert!(result.is_ok(), "wait should succeed after 3 reads");
        assert_eq!(provider.read_count(), 3);
    }

    #[tokio::test]
    async fn wait_returns_timeout_when_predicate_stays_false() {
        let provider = ReadSequenceProvider::new(vec![state_with_status("PENDING_VALIDATION")]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_millis(10),
            Duration::from_millis(2),
        )
        .await;
        let err = result.expect_err("should time out");
        assert!(
            matches!(err, ProviderError::Timeout(_)),
            "expected Timeout, got {:?}",
            err
        );
        assert!(
            err.to_string().contains("PENDING_VALIDATION"),
            "error message should include last observed value, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn wait_returns_not_found_when_target_disappears() {
        let provider = ReadSequenceProvider::new(vec![
            state_with_status("PENDING_VALIDATION"),
            State::not_found(ResourceId::new("acm.Certificate", "cert")),
        ]);
        let pred = equals_status("ISSUED");
        let target = ResourceId::new("acm.Certificate", "cert");
        let result = execute_wait_effect(
            &provider,
            &target,
            None,
            &pred,
            Duration::from_secs(60),
            Duration::from_millis(1),
        )
        .await;
        let err = result.expect_err("should fail with NotFound");
        assert!(
            matches!(err, ProviderError::NotFound(_)),
            "expected NotFound, got {:?}",
            err
        );
    }
}
