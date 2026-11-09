use crate::{EventLevel, ProviderValueDiagnostic};
use carina_core::provider::ProviderError;
#[cfg(not(target_arch = "wasm32"))]
use carina_core::provider::{ProviderSession, ProviderValidationInputs};
#[cfg(not(target_arch = "wasm32"))]
use carina_core::provider_text::ProviderDiagnosticChannel;
use carina_core::provider_text::{ProviderContext, ProviderDiagnostic};
use carina_provider_protocol::diagnostics as wire;

/// Runtime-owned emission capability, obtained from a native session or guest runtime.
///
/// ```compile_fail,E0277
/// let _: carina_plugin_sdk::ProviderDiagnostics = Default::default();
/// ```
/// ```compile_fail,E0599
/// let _ = carina_plugin_sdk::ProviderDiagnostics::new(std::io::sink());
/// ```
/// ```
/// use carina_core::provider::ProviderPlan;
/// use carina_plugin_sdk::ProviderDiagnostics;
/// let session = ProviderPlan::state_only().session("example", None, None);
/// let diagnostics = ProviderDiagnostics::from_session(&session);
/// ```
#[derive(Clone)]
pub struct ProviderDiagnostics {
    backend: Backend,
}
#[derive(Clone)]
enum Backend {
    #[cfg(not(target_arch = "wasm32"))]
    Native(ProviderDiagnosticChannel),
    #[cfg(target_arch = "wasm32")]
    Guest,
}
impl ProviderDiagnostics {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_session(session: &ProviderSession) -> Self {
        Self {
            backend: Backend::Native(session.diagnostics()),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub fn for_validation(inputs: &ProviderValidationInputs) -> Self {
        Self {
            backend: Backend::Native(inputs.diagnostics()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    pub fn guest() -> Self {
        install_panic_hook();
        Self {
            backend: Backend::Guest,
        }
    }

    /// Emission accepts no formatter, raw string, or arbitrary error type.
    ///
    /// ```compile_fail,E0308
    /// use carina_plugin_sdk::ProviderDiagnostics;
    /// fn reject(d: &ProviderDiagnostics) { d.emit(String::from("raw")); }
    /// ```
    /// ```compile_fail,E0308
    /// use carina_plugin_sdk::ProviderDiagnostics;
    /// fn reject(d: &ProviderDiagnostics) { d.emit(format_args!("raw {}", 123)); }
    /// ```
    /// ```
    /// use carina_core::provider::{ProviderSession, ProviderError};
    /// use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
    /// fn emit(session: &ProviderSession) {
    ///     let diagnostics = ProviderDiagnostics::from_session(session);
    ///     let error = ProviderError::internal("ordinary handled failure");
    ///     diagnostics.emit(ProviderEvent::failure(EventLevel::Warn, "retry".into(), &error));
    /// }
    /// ```
    pub fn emit(&self, event: ProviderEvent<'_>) {
        match (&self.backend, event.0) {
            #[cfg(not(target_arch = "wasm32"))]
            (Backend::Native(channel), event) => channel.emit(event),
            #[cfg(target_arch = "wasm32")]
            (Backend::Guest, event) => guest::emit(event.into_wire()),
        }
    }
    pub(crate) fn register(
        &self,
        material: wire::ProtectedMaterial,
    ) -> Result<(), wire::RegistrationError> {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Native(channel) => channel.register_protected(material),
            #[cfg(target_arch = "wasm32")]
            Backend::Guest => guest::register(material),
        }
    }
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn http(&self, http: wire::HttpDiagnostic) {
        guest::emit(wire::Event {
            severity: EventLevel::Trace,
            context: None,
            body: wire::Payload::Http(http),
        });
    }
}

// Keep this SDK authoring wrapper: core's transport encoder is public on WASM,
// so re-exporting it here would expose wire extraction through the SDK event.
pub struct ProviderEvent<'a>(carina_core::provider_text::ProviderEvent<'a>);
impl<'a> ProviderEvent<'a> {
    pub fn failure(level: EventLevel, context: ProviderContext, error: &'a ProviderError) -> Self {
        Self(carina_core::provider_text::ProviderEvent::failure(
            level, context, error,
        ))
    }
    pub fn diagnostic(
        level: EventLevel,
        context: ProviderContext,
        diagnostic: &'a ProviderDiagnostic,
    ) -> Self {
        Self(carina_core::provider_text::ProviderEvent::diagnostic(
            level, context, diagnostic,
        ))
    }
    pub fn value(level: EventLevel, value: ProviderValueDiagnostic<'a>) -> Self {
        Self(carina_core::provider_text::ProviderEvent::value(
            level, value,
        ))
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let message =
                if let Some(diagnostic) = info.payload().downcast_ref::<ProviderDiagnostic>() {
                    Some(
                        ProviderError::from_diagnostic(
                            carina_core::provider::ProviderErrorKind::Internal,
                            diagnostic.clone(),
                        )
                        .to_wire()
                        .message,
                    )
                } else if let Some(text) = info.payload().downcast_ref::<String>() {
                    Some(text.clone())
                } else {
                    info.payload()
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                };
            let location = info.location();
            guest::emit(wire::Event {
                severity: EventLevel::Error,
                context: None,
                body: wire::Payload::Panic(wire::PanicDiagnostic {
                    message,
                    file: location.map(|location| location.file().to_owned()),
                    line: location.map(std::panic::Location::line),
                    column: location.map(std::panic::Location::column),
                }),
            });
        }))
    });
}

/// Panic with an opaque diagnostic rather than formatting an error with Debug.
#[track_caller]
pub fn panic_diagnostic(diagnostic: ProviderDiagnostic) -> ! {
    std::panic::panic_any(diagnostic)
}

/// Preserve a typed failure through the SDK panic hook.
#[track_caller]
pub fn expect_diagnostic<T>(result: Result<T, ProviderDiagnostic>) -> T {
    match result {
        Ok(value) => value,
        Err(diagnostic) => panic_diagnostic(diagnostic),
    }
}

#[cfg(target_arch = "wasm32")]
mod guest;
