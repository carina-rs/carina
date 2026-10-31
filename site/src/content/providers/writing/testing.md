---
title: "Testing a Provider"
description: "Unit-test a Carina provider's operations against a fake API on the host target, then run acceptance tests that apply real configurations."
---

A provider is tested at two levels. Unit tests run its operations on the host target against a fake API and check the outcomes Carina will see. Acceptance tests build the component and run `carina` against a real account, which is the only way to check that a resource converges.

## Unit tests

The [`widget` provider](/providers/writing/implementing-operations/) takes its API client as a type parameter, so tests can give it a fake. They are ordinary Rust tests and run with `cargo test` on the host; you do not need the `wasm32-wasip2` target for them.

There is one wrinkle. A `CreateAttempt` can only be created by the SDK, so a test cannot call `create` directly. Call `carina_plugin_sdk::dispatch_create` instead: it is the entry point the component's generated code uses, so it mints the attempt and returns the outcome in the wire form the host receives. `dispatch_poll_create` and `dispatch_resume_create` do the same for the other create calls. `update`, `delete` and `read` take no typestate, so tests call them directly.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use carina_plugin_sdk::protocol::types::{
        CreateStep as WireCreateStep, CreateVerdict as WireCreateVerdict,
        PollVerdict as WirePollVerdict,
    };
    use carina_plugin_sdk::{PollFinality, PollRequest};
    use std::cell::RefCell;

    /// An in-memory API that records what the provider asked for.
    #[derive(Default)]
    struct FakeApi {
        widgets: RefCell<Vec<Widget>>,
        /// Client tokens already used, with the operation each started.
        operations: RefCell<HashMap<String, String>>,
        /// Errors to return from the next start_create calls, in order.
        start_errors: RefCell<Vec<ApiError>>,
        size_calls: RefCell<Vec<(String, i64)>>,
    }

    impl Api for FakeApi {
        fn new_client_token(&self) -> String {
            "token-1".to_string()
        }
        fn start_create(
            &self,
            client_token: &str,
            name: &str,
            size: i64,
        ) -> Result<String, ApiError> {
            if let Some(operation) = self.operations.borrow().get(client_token) {
                return Ok(operation.clone());
            }
            if !self.start_errors.borrow().is_empty() {
                return Err(self.start_errors.borrow_mut().remove(0));
            }
            self.widgets.borrow_mut().push(Widget {
                id: format!("w-{name}"),
                name: name.into(),
                size,
            });
            let operation = format!("op-{name}");
            self.operations
                .borrow_mut()
                .insert(client_token.into(), operation.clone());
            Ok(operation)
        }
        fn create_status(&self, operation: &str) -> Result<CreateStatus, ApiError> {
            let name = operation.trim_start_matches("op-");
            Ok(CreateStatus::Done {
                id: format!("w-{name}"),
            })
        }
        fn get(&self, id: &str) -> Result<Widget, ApiError> {
            self.widgets
                .borrow()
                .iter()
                .find(|w| w.id == id)
                .cloned()
                .ok_or(ApiError::NotFound)
        }
        fn set_size(&self, id: &str, size: i64) -> Result<(), ApiError> {
            self.size_calls.borrow_mut().push((id.to_string(), size));
            Ok(())
        }
        fn delete(&self, _id: &str) -> Result<(), ApiError> {
            Err(ApiError::NotFound)
        }
    }

    fn api_failing_with(errors: Vec<ApiError>) -> FakeApi {
        FakeApi {
            start_errors: RefCell::new(errors),
            ..Default::default()
        }
    }

    /// Poll a pending create once, the way the host does.
    fn poll(
        provider: &WidgetProvider<FakeApi>,
        pending: carina_plugin_sdk::protocol::types::PendingCreate<CreateToken>,
    ) -> WireCreateStep<HashMap<String, Value>, PartialReadDiagnostic, CreateToken> {
        carina_plugin_sdk::dispatch_poll_create(
            provider,
            &resource_id(),
            create_request("a", 1),
            pending.identifier,
            pending.sent,
            pending.token,
            PollRequest { finality: PollFinality::Continue },
        )
        .expect("poll should not fail")
    }

    fn resource_id() -> ResourceId {
        ResourceId {
            provider: "widget".into(),
            resource_type: "widget".into(),
            identity: "main".into(),
        }
    }

    fn create_request(name: &str, size: i64) -> CreateRequest {
        CreateRequest {
            resource: Resource {
                id: resource_id(),
                attributes: HashMap::from([
                    ("name".to_string(), Value::String(name.into())),
                    ("size".to_string(), Value::Int(size)),
                ]),
                directives: Default::default(),
            },
        }
    }

    #[test]
    fn rejected_create_is_untracked() {
        let provider = WidgetProvider {
            api: api_failing_with(vec![ApiError::Rejected("size too large".into())]),
        };
        // `CreateAttempt` can only be minted by the SDK; dispatch_create
        // goes through the same entry point the host uses.
        let verdict =
            carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 99))
                .expect_err("a rejected create must not succeed");
        assert!(matches!(*verdict, WireCreateVerdict::Untracked(_)));
    }

    #[test]
    fn create_with_unknown_outcome_is_retried_with_the_same_client_token() {
        let provider = WidgetProvider {
            api: api_failing_with(vec![ApiError::Unknown("connection reset".into())]),
        };
        let step =
            carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 1))
                .expect("an unknown outcome must not be reported as a failure");
        let WireCreateStep::Pending(pending) = step else {
            panic!("expected pending")
        };
        assert!(pending.identifier.is_none());

        // The retry starts the create once; no second widget appears.
        let WireCreateStep::Pending(pending) = poll(&provider, pending) else {
            panic!("expected pending")
        };
        assert!(matches!(pending.token, CreateToken::Operation(_)));
        assert_eq!(provider.api.widgets.borrow().len(), 1);
    }

    #[test]
    fn rejected_retry_preserves_the_continuation() {
        let provider = WidgetProvider {
            api: api_failing_with(vec![
                ApiError::Unknown("timeout".into()),
                ApiError::Rejected("client token reused with different parameters".into()),
            ]),
        };
        let step =
            carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 1))
                .expect("an unknown outcome must not be reported as a failure");
        let WireCreateStep::Pending(pending) = step else {
            panic!("expected pending")
        };
        let verdict = carina_plugin_sdk::dispatch_poll_create(
            &provider,
            &resource_id(),
            create_request("a", 1),
            pending.identifier,
            pending.sent,
            pending.token,
            PollRequest { finality: PollFinality::Continue },
        )
        .expect_err("a rejected retry must not succeed");
        let WirePollVerdict::InFlight(in_flight) = *verdict else {
            panic!("expected an in-flight verdict")
        };
        match in_flight.pending.token {
            CreateToken::Starting { client_token } => {
                assert_eq!(client_token, "token-1");
            }
            CreateToken::Operation(operation) => {
                panic!("expected the starting continuation, got {operation}");
            }
        }
    }

    #[test]
    fn poll_records_the_identifier_then_reads_the_widget() {
        let provider = WidgetProvider {
            api: FakeApi::default(),
        };
        let step =
            carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 1))
                .expect("create should start");
        let WireCreateStep::Pending(pending) = step else {
            panic!("expected pending")
        };

        let WireCreateStep::Pending(pending) = poll(&provider, pending) else {
            panic!("expected pending")
        };
        assert_eq!(
            pending.identifier.as_ref().map(|id| id.as_str()),
            Some("w-a")
        );

        let WireCreateStep::Success {
            identifier,
            attributes,
        } = poll(&provider, pending)
        else {
            panic!("expected success")
        };
        assert_eq!(identifier.as_str(), "w-a");
        assert!(matches!(attributes.get("size"), Some(Value::Int(1))));
    }

    #[test]
    fn update_sends_only_patched_keys() {
        let provider = WidgetProvider {
            api: FakeApi::default(),
        };
        let identifier = AppliedIdentifier::new("w-a").unwrap();
        let request = UpdateRequest {
            from: PersistedState {
                id: resource_id(),
                identifier: Some("w-a".into()),
                attributes: HashMap::from([
                    ("name".to_string(), PersistedValue::String("a".into())),
                    ("size".to_string(), PersistedValue::Int(1)),
                ]),
                exists: true,
            },
            patch: UpdatePatch {
                ops: vec![PatchOp::Replace {
                    key: "size".into(),
                    value: Value::Int(2),
                }],
            },
        };
        provider
            .update(&resource_id(), &identifier, request)
            .expect("update should succeed");
        assert_eq!(
            *provider.api.size_calls.borrow(),
            vec![("w-a".to_string(), 2)]
        );
    }

    #[test]
    fn deleting_a_missing_widget_succeeds() {
        let provider = WidgetProvider {
            api: FakeApi::default(),
        };
        let identifier = AppliedIdentifier::new("w-gone").unwrap();
        let outcome = provider.delete(&resource_id(), &identifier, DeleteRequest::default());
        assert!(matches!(outcome, Ok(DeleteOutcome::Deleted)));
    }
}
```

Test the classification as well as the happy path. Tests like `create_with_unknown_outcome_is_retried_with_the_same_client_token` and `rejected_retry_preserves_the_continuation` are what keep a later change from turning an ambiguous failure into `untracked` and losing track of a real resource.

## Acceptance tests

Unit tests cannot tell you whether a resource converges: whether, after `apply`, the attributes the provider returns from `read` match the configuration so that the next plan is empty. That needs the real API.

The official providers keep acceptance tests in an `acceptance-tests/` directory, one directory per resource type with one or more `.crn` configurations, and a script that runs each one through a full cycle:

1. `carina init` installs the component under test. Run it again after every rebuild; otherwise the test runs the copy installed earlier.
2. `carina apply --auto-approve` creates the resources.
3. `carina plan --detailed-exitcode` must exit with `0`. It exits with `2` when the plan has changes, which means the resource does not converge, usually because `read` returns a value in a different form than the configuration or leaves an attribute out.
4. `carina destroy --auto-approve` removes everything.

Updates, attribute removal and replacement need more than one configuration. Keep the steps outside the working directory, such as `step1.crn` and `step2.crn`, and copy each in turn over `main.crn` in one working directory, so the second apply updates what the first created. Do not put the step files in the working directory itself: Carina reads every `.crn` file in a directory, so it would declare the resources twice. Check the plan between steps for the change you expect, and run the converge check after each apply.

Run acceptance tests in an account that holds nothing else, and make the script destroy what it created even when a step fails or the run is interrupted. A test that leaves resources behind costs money and can break the next run. Point `source` at the build under test with a `file://` URL, which step 1 installs, and use a release build of `carina`: loading a component in a debug build is slow.

A local build receives no AWS credentials or metadata access by default. For an AWS or AWSCC acceptance run, set `CARINA_TRUST_LOCAL_PROVIDER=/absolute/path/to/provider.wasm=aws` for every `apply`, `plan` and `destroy` command, or pass the equivalent repeatable `--trust-local-provider` flag to each command. The environment variable separates multiple entries with `:` on Unix and `;` on Windows. Re-run `carina init` after rebuilding so the installed bytes still match the granted source file.

## Next steps

[Versioning and Compatibility](/providers/writing/versioning/) explains which Carina releases can load your provider and what to do when the protocol changes. The protocol-level details behind these pages are in [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md).
