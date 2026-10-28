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
    };
    use std::cell::RefCell;

    /// An in-memory API that records what the provider asked for.
    #[derive(Default)]
    struct FakeApi {
        widgets: RefCell<Vec<Widget>>,
        fail_create: Option<fn() -> ApiError>,
        size_calls: RefCell<Vec<(String, i64)>>,
    }

    impl Api for FakeApi {
        fn start_create(&self, name: &str, size: i64) -> Result<String, ApiError> {
            if let Some(fail) = self.fail_create {
                return Err(fail());
            }
            self.widgets.borrow_mut().push(Widget { id: format!("w-{name}"), name: name.into(), size });
            Ok(format!("op-{name}"))
        }
        fn create_status(&self, operation: &str) -> Result<CreateStatus, ApiError> {
            let name = operation.trim_start_matches("op-");
            Ok(CreateStatus::Done { id: format!("w-{name}") })
        }
        fn find_by_name(&self, name: &str) -> Result<Option<Widget>, ApiError> {
            Ok(self.widgets.borrow().iter().find(|w| w.name == name).map(|w| Widget {
                id: w.id.clone(),
                name: w.name.clone(),
                size: w.size,
            }))
        }
        fn get(&self, id: &str) -> Result<Widget, ApiError> {
            self.widgets
                .borrow()
                .iter()
                .find(|w| w.id == id)
                .map(|w| Widget { id: w.id.clone(), name: w.name.clone(), size: w.size })
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
            api: FakeApi {
                fail_create: Some(|| ApiError::Rejected("size too large".into())),
                ..Default::default()
            },
        };
        // `CreateAttempt` can only be minted by the SDK; dispatch_create
        // goes through the same entry point the host uses.
        let verdict = carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 99))
            .expect_err("a rejected create must not succeed");
        assert!(matches!(*verdict, WireCreateVerdict::Untracked(_)));
    }

    #[test]
    fn create_with_unknown_outcome_stays_pending() {
        let provider = WidgetProvider {
            api: FakeApi {
                fail_create: Some(|| ApiError::Unknown("connection reset".into())),
                ..Default::default()
            },
        };
        let step = carina_plugin_sdk::dispatch_create(&provider, &resource_id(), create_request("a", 1))
            .expect("an unknown outcome must not be reported as a failure");
        assert!(matches!(step, WireCreateStep::Pending(pending) if pending.identifier.is_none()));
    }

    #[test]
    fn poll_finishes_a_create_with_the_identifier() {
        let provider = WidgetProvider { api: FakeApi::default() };
        let id = resource_id();
        let step = carina_plugin_sdk::dispatch_create(&provider, &id, create_request("a", 1))
            .expect("create should start");
        let WireCreateStep::Pending(pending) = step else { panic!("expected pending") };
        let step = carina_plugin_sdk::dispatch_poll_create(
            &provider,
            &id,
            create_request("a", 1),
            pending.identifier,
            pending.sent,
            pending.token,
        )
        .expect("poll should finish");
        let WireCreateStep::Success { identifier, attributes } = step else { panic!("expected success") };
        assert_eq!(identifier.as_str(), "w-a");
        assert!(matches!(attributes.get("size"), Some(Value::Int(1))));
    }

    #[test]
    fn update_sends_only_patched_keys() {
        let provider = WidgetProvider { api: FakeApi::default() };
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
                ops: vec![PatchOp::Replace { key: "size".into(), value: Value::Int(2) }],
            },
        };
        provider.update(&resource_id(), &identifier, request).expect("update should succeed");
        assert_eq!(*provider.api.size_calls.borrow(), vec![("w-a".to_string(), 2)]);
    }

    #[test]
    fn deleting_a_missing_widget_succeeds() {
        let provider = WidgetProvider { api: FakeApi::default() };
        let identifier = AppliedIdentifier::new("w-gone").unwrap();
        let outcome = provider.delete(&resource_id(), &identifier, DeleteRequest::default());
        assert!(matches!(outcome, Ok(DeleteOutcome::Deleted)));
    }
}
```

Test the classification as well as the happy path. A test like `create_with_unknown_outcome_stays_pending` is what keeps a later change from turning an ambiguous failure into `untracked` and losing track of a real resource.

## Acceptance tests

Unit tests cannot tell you whether a resource converges: whether, after `apply`, the attributes the provider returns from `read` match the configuration so that the next plan is empty. That needs the real API.

The official providers keep acceptance tests in an `acceptance-tests/` directory, one directory per resource type with one or more `.crn` configurations, and a script that runs each one through a full cycle:

1. `carina apply --auto-approve` creates the resources.
2. `carina plan --detailed-exitcode` must exit with `0`. It exits with `2` when the plan has changes, which means the resource does not converge, usually because `read` returns a value in a different form than the configuration or leaves an attribute out.
3. `carina destroy --auto-approve` removes everything.

Updates, attribute removal and replacement need more than one configuration. Write the steps as separate files, such as `step1.crn` and `step2.crn`, copy each in turn into the same directory as `main.crn`, and apply them in order so the second apply updates what the first created. Check the plan between steps for the change you expect, and run the converge check after each apply.

Run acceptance tests in an account that holds nothing else, and make the script destroy what it created even when a step fails or the run is interrupted. A test that leaves resources behind costs money and can break the next run. Point `source` at the build under test with a `file://` URL, and use a release build of `carina`: loading a component in a debug build is slow.

## Next steps

The protocol-level details behind these pages are in [docs/protocol.md](https://github.com/carina-rs/carina/blob/main/docs/protocol.md).
