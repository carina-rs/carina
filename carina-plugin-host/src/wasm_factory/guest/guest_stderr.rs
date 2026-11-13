//! Host-owned diagnostic output. This type has no production WASI stream capability. Retain a bounded sanitized prefix
//! and count the entire discarded tail before emitting one line at its end.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use carina_core::provider::ProviderValidationInputs;
use carina_core::provider_text::GuestInputInventory;
use carina_core::provider_text::RedactingLineWriter;
use carina_core::provider_text::RedactionSet;

/// All handles for one guest stderr share a buffer and a host-owned sink.
#[derive(Clone)]
pub(in crate::wasm_factory) struct HostOutput {
    state: Arc<Mutex<LineBuffer>>,
}

struct LineBuffer {
    writer: Option<RedactingLineWriter<SharedSink>>,
    session: Option<ProviderValidationInputs>,
    sink: SharedSink,
    failed: bool,
}
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Box<dyn Write + Send>>>);
impl Write for SharedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush()
    }
}
impl HostOutput {
    pub(in crate::wasm_factory) fn new(sink: impl Write + Send + 'static) -> Self {
        let sink = SharedSink(Arc::new(Mutex::new(Box::new(sink))));
        Self {
            state: Arc::new(Mutex::new(LineBuffer {
                writer: Some(RedactingLineWriter::for_metadata(sink.clone())),
                session: None,
                sink,
                failed: false,
            })),
        }
    }
    #[cfg(test)]
    pub(in crate::wasm_factory) fn begin_call(
        &self,
        session: &ProviderValidationInputs,
    ) -> HostOutputCall {
        self.begin_inventory_call(session, &session.guest_inventory())
    }
    pub(in crate::wasm_factory) fn begin_inventory_call(
        &self,
        session: &ProviderValidationInputs,
        inputs: &GuestInputInventory,
    ) -> HostOutputCall {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = state.flush_pending();
        state.session = Some(session.clone());
        state.writer = Some(inputs.line_writer(state.sink.clone()));
        HostOutputCall(self.clone())
    }
    pub(in crate::wasm_factory) fn known_secrets(&self) -> Option<RedactionSet> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session
            .as_ref()
            .filter(|session| !session.guest_inventory().is_empty())
            .map(|session| session.redaction_set().clone())
    }

    pub(in crate::wasm_factory) fn write_line(&self, text: &str) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ensure_open()?;
        state.push_str(text)?;
        state.push_str("\n")?;
        state.flush_pending()
    }

    fn flush_pending(&self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_pending()
    }
}

pub(in crate::wasm_factory) struct HostOutputCall(HostOutput);
impl Drop for HostOutputCall {
    fn drop(&mut self) {
        let _ = self.0.flush_pending();
    }
}
impl Drop for LineBuffer {
    fn drop(&mut self) {
        let _ = self.flush_pending();
    }
}
impl LineBuffer {
    fn push_str(&mut self, text: &str) -> io::Result<()> {
        let result = self
            .writer
            .as_mut()
            .expect("stderr writer installed")
            .push_str(text);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn ensure_open(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "guest stderr sink is closed",
            ))
        } else {
            Ok(())
        }
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        self.writer
            .as_mut()
            .expect("stderr writer installed")
            .finish()?;
        let result = self.sink.flush();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_text_fixtures::*;

    mod deferred_input_redaction {
        include!("guest_stderr/tests/deferred_input_redaction.rs");
    }

    struct StderrConfigFactory;
    impl carina_core::provider::ProviderFactory for StderrConfigFactory {
        fn name(&self) -> &str {
            "stderr-test"
        }
        fn display_name(&self) -> &str {
            "stderr test"
        }
        fn validate_config(
            &self,
            _: &carina_core::provider::CanonicalProviderConfigAttributes,
            _session: &carina_core::provider::ProviderValidationInputs,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }

        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            Vec::new()
        }
        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                Vec::new(),
                {
                    carina_core::schema::ValidatedConfigSchema::validate(
                        std::collections::BTreeMap::new(),
                    )
                    .unwrap()
                },
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }
        fn create_provider(
            &self,
            _: &carina_core::provider::ProviderSession,
            _: carina_core::provider::InitializeOutcomeMint,
        ) -> carina_core::provider::BoxFuture<
            '_,
            carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
        > {
            unreachable!("stderr test only prepares config")
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn call_secrets(plaintext: &str) -> ProviderValidationInputs {
        crate::provider_text_fixtures::context(&[plaintext])
            .session()
            .guest_inputs()
    }
    #[test]
    fn review_a1_poison_relay_does_not_rescan_inserted_masks() {
        let context = call_secrets("secret");
        let stderr = HostOutput::new(Capture::default());
        drop(stderr.begin_call(&context));
        let reason = super::super::PoisonReason::from_trap(
            "create",
            &wasmtime::Error::msg("secret"),
            stderr.known_secrets().as_ref(),
        );
        let error = reason
            .fail_fast_error("delete")
            .with_secrets_for_test(&crate::provider_text_fixtures::context(&["secret"]));
        assert_eq!(
            error.to_string(),
            "WASM provider instance unusable after trap in create: (secret) (operation 'delete'); re-run the command"
        );
    }

    #[test]
    fn review_a2_metadata_relay_retains_prior_call_context() {
        let context =
            crate::provider_text_fixtures::context(&["prior-private", "different-private"]);
        let cause =
            crate::wasm_factory::MetadataDiagnostic::new(std::io::Error::other("prior-private"));
        let relayed =
            carina_core::provider::ProviderError::internal_caused_by("public context", cause);
        let received = relayed.with_secrets_for_test(&context);
        assert_eq!(received.to_string(), "public context: (secret)");
    }

    #[test]
    fn guest_stderr_known_secrets_protect_poisoned_instance_diagnostics() {
        let stderr = HostOutput::new(Capture::default());
        let call = stderr.begin_call(&call_secrets("previous-call-private"));
        drop(call);
        let reason = super::super::PoisonReason::from_trap(
            "create",
            &wasmtime::Error::msg("previous-call-private"),
            stderr.known_secrets().as_ref(),
        );
        assert!(!format!("{reason:?}").contains("previous-call-private"));
        let error = reason.fail_fast_error("delete").with_secrets_for_test(
            &crate::provider_text_fixtures::context(&["previous-call-private"]),
        );
        assert!(error.to_string().contains("(secret)"));
        assert!(!error.to_string().contains("previous-call-private"));
    }
}
