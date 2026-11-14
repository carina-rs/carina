//! Backend values have separate authored, retaining and strict phases.
use super::{ConfigurationAttributes, ParseError};
use crate::resource::{BindingAddress, Value, ValueKind};
use std::{fmt::Debug, sync::Arc};

mod sealed {
    pub trait Sealed {}
}

#[derive(Debug, Clone, Copy)]
pub struct AuthoredBackend;
#[derive(Debug, Clone, Copy)]
pub struct RetainedBackend;
#[derive(Debug, Clone, Copy)]
pub struct StrictBackend;
impl sealed::Sealed for AuthoredBackend {}
impl sealed::Sealed for RetainedBackend {}
impl sealed::Sealed for StrictBackend {}

/// A retained evaluator result. This phase is never serializable. A plain
/// value cannot silently acquire an evaluation result marker.
///
/// ```compile_fail,E0277
/// use carina_core::parser::BackendAttributeValue;
/// use carina_core::resource::{ConcreteValue, NoPlaceholder, Value};
/// let value: Value<NoPlaceholder> = Value::Concrete(ConcreteValue::Bool(true));
/// let evaluated: BackendAttributeValue = value.into();
/// ```
#[derive(Debug, Clone)]
pub enum BackendAttributeValue<
    P: ValueKind = crate::resource::NoPlaceholder,
    B: BindingAddress = crate::resource::RelativeBindingName,
> {
    Evaluated(Value<P, B>),
    Failed(Arc<ParseError>),
}

/// The authored operands needed when a later owning scope evaluates a new phase.
#[derive(Debug, Clone)]
pub struct BackendSource(pub(crate) ConfigurationAttributes<BackendExpression>);

/// A parser-owned expression capability. Capturing it performs no evaluation.
#[derive(Clone)]
pub struct BackendExpression {
    pub(super) file: Arc<super::PreprocessedFile>,
    pub(super) location: super::DeclarationLocation,
    pub(super) nodes: Arc<super::retained::CapturedNodes>,
    pub(super) start: usize,
    pub(super) end: usize,
}
impl Debug for BackendExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendExpression")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}
impl BackendExpression {
    pub(super) fn needs_bindings(
        &self,
        functions: &indexmap::IndexMap<String, super::UserFunction>,
        functions_available: bool,
    ) -> Result<bool, ParseError> {
        self.with_expression(|pair| {
            Ok(pair
                .into_inner()
                .flatten()
                .any(|part| match part.as_rule() {
                    super::Rule::variable_ref
                    | super::Rule::namespaced_id
                    | super::Rule::subscripted_id => true,
                    super::Rule::function_call => part.into_inner().next().is_some_and(|name| {
                        functions.contains_key(name.as_str())
                            || (!functions_available
                                && !crate::builtins::is_known_builtin(name.as_str()))
                    }),
                    _ => false,
                }))
        })
    }
    pub(super) fn with_expression<T>(
        &self,
        evaluate: impl FnOnce(pest::iterators::Pair<'_, super::Rule>) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let expression = self
            .file
            .parse()?
            .flatten()
            .find(|pair| {
                pair.as_rule() == super::Rule::expression
                    && pair.as_span().start() == self.start
                    && pair.as_span().end() == self.end
            })
            .ok_or_else(|| ParseError::InternalError {
                expected: "parser-owned backend expression".into(),
                context: "backend evaluation".into(),
            })?;
        evaluate(expression)
    }
}

/// Backend traversal preserves the phase and every retained failure. Value
/// visitors receive ordinary Values; they cannot turn a Failed into absence.
pub trait BackendPhase: sealed::Sealed + Clone + Debug + 'static {
    type Attribute<P: ValueKind, B: BindingAddress>: Clone + Debug;
    type Source<P: ValueKind, B: BindingAddress>: Clone + Debug;
    fn try_map_attribute<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        attribute: Self::Attribute<P, B>,
        map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<Self::Attribute<Q, C>, E>;
    fn try_map_source<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        source: Self::Source<P, B>,
        map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<Self::Source<Q, C>, E>;
    fn try_visit_attribute<'a, P: ValueKind + 'a, B: BindingAddress, E>(
        attribute: &'a Self::Attribute<P, B>,
        visit: &mut impl FnMut(&'a Value<P, B>) -> Result<(), E>,
    ) -> Result<(), E>;
    /// Only strict values can continue a deferred call when its session becomes
    /// available. Authored operands and retained outcomes have their own owner.
    fn try_continue_attribute<P: ValueKind, B: BindingAddress, E>(
        attribute: &mut Self::Attribute<P, B>,
        evaluate: &mut impl FnMut(&Value<P, B>) -> Result<Value<P, B>, E>,
    ) -> Result<(), E>;
    fn visit_attribute_mut<P: ValueKind, B: BindingAddress>(
        attribute: &mut Self::Attribute<P, B>,
        visit: &mut impl FnMut(&mut Value<P, B>),
    );
    fn visit_source_mut<P: ValueKind, B: BindingAddress>(
        source: &mut Self::Source<P, B>,
        visit: &mut impl FnMut(&mut Value<P, B>),
    );
}
macro_rules! plain_phase {
    ($phase:ty) => {
        impl BackendPhase for $phase {
            type Attribute<P: ValueKind, B: BindingAddress> = Value<P, B>;
            type Source<P: ValueKind, B: BindingAddress> = ();
            fn try_map_attribute<
                P: ValueKind,
                Q: ValueKind,
                B: BindingAddress,
                C: BindingAddress,
                E,
            >(
                attribute: Value<P, B>,
                map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
            ) -> Result<Value<Q, C>, E> {
                map(attribute)
            }
            fn try_map_source<
                P: ValueKind,
                Q: ValueKind,
                B: BindingAddress,
                C: BindingAddress,
                E,
            >(
                (): (),
                _map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
            ) -> Result<(), E> {
                Ok(())
            }
            fn try_visit_attribute<'a, P: ValueKind + 'a, B: BindingAddress, E>(
                attribute: &'a Value<P, B>,
                visit: &mut impl FnMut(&'a Value<P, B>) -> Result<(), E>,
            ) -> Result<(), E> {
                visit(attribute)
            }
            fn try_continue_attribute<P: ValueKind, B: BindingAddress, E>(
                attribute: &mut Value<P, B>,
                evaluate: &mut impl FnMut(&Value<P, B>) -> Result<Value<P, B>, E>,
            ) -> Result<(), E> {
                *attribute = evaluate(attribute)?;
                Ok(())
            }
            fn visit_attribute_mut<P: ValueKind, B: BindingAddress>(
                attribute: &mut Value<P, B>,
                visit: &mut impl FnMut(&mut Value<P, B>),
            ) {
                visit(attribute);
            }
            fn visit_source_mut<P: ValueKind, B: BindingAddress>(
                (): &mut (),
                _visit: &mut impl FnMut(&mut Value<P, B>),
            ) {
            }
        }
    };
}
impl BackendPhase for AuthoredBackend {
    type Attribute<P: ValueKind, B: BindingAddress> = BackendExpression;
    type Source<P: ValueKind, B: BindingAddress> = ();
    fn try_map_attribute<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        attribute: BackendExpression,
        _map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<BackendExpression, E> {
        Ok(attribute)
    }
    fn try_map_source<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        (): (),
        _map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<(), E> {
        Ok(())
    }
    fn try_visit_attribute<'a, P: ValueKind + 'a, B: BindingAddress, E>(
        _attribute: &'a BackendExpression,
        _visit: &mut impl FnMut(&'a Value<P, B>) -> Result<(), E>,
    ) -> Result<(), E> {
        Ok(())
    }
    fn try_continue_attribute<P: ValueKind, B: BindingAddress, E>(
        _attribute: &mut BackendExpression,
        _evaluate: &mut impl FnMut(&Value<P, B>) -> Result<Value<P, B>, E>,
    ) -> Result<(), E> {
        Ok(())
    }
    fn visit_attribute_mut<P: ValueKind, B: BindingAddress>(
        _attribute: &mut BackendExpression,
        _visit: &mut impl FnMut(&mut Value<P, B>),
    ) {
    }
    fn visit_source_mut<P: ValueKind, B: BindingAddress>(
        (): &mut (),
        _visit: &mut impl FnMut(&mut Value<P, B>),
    ) {
    }
}
plain_phase!(StrictBackend);
impl BackendPhase for RetainedBackend {
    type Attribute<P: ValueKind, B: BindingAddress> = BackendAttributeValue<P, B>;
    type Source<P: ValueKind, B: BindingAddress> = BackendSource;
    fn try_map_attribute<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        attribute: Self::Attribute<P, B>,
        map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<Self::Attribute<Q, C>, E> {
        match attribute {
            BackendAttributeValue::Evaluated(value) => {
                map(value).map(BackendAttributeValue::Evaluated)
            }
            BackendAttributeValue::Failed(cause) => Ok(BackendAttributeValue::Failed(cause)),
        }
    }
    fn try_map_source<P: ValueKind, Q: ValueKind, B: BindingAddress, C: BindingAddress, E>(
        source: BackendSource,
        _map: &mut impl FnMut(Value<P, B>) -> Result<Value<Q, C>, E>,
    ) -> Result<BackendSource, E> {
        Ok(source)
    }
    fn try_visit_attribute<'a, P: ValueKind + 'a, B: BindingAddress, E>(
        attribute: &'a BackendAttributeValue<P, B>,
        visit: &mut impl FnMut(&'a Value<P, B>) -> Result<(), E>,
    ) -> Result<(), E> {
        match attribute {
            BackendAttributeValue::Evaluated(value) => visit(value),
            BackendAttributeValue::Failed(_) => Ok(()),
        }
    }
    fn try_continue_attribute<P: ValueKind, B: BindingAddress, E>(
        _attribute: &mut Self::Attribute<P, B>,
        _evaluate: &mut impl FnMut(&Value<P, B>) -> Result<Value<P, B>, E>,
    ) -> Result<(), E> {
        Ok(())
    }
    fn visit_attribute_mut<P: ValueKind, B: BindingAddress>(
        attribute: &mut Self::Attribute<P, B>,
        visit: &mut impl FnMut(&mut Value<P, B>),
    ) {
        match attribute {
            BackendAttributeValue::Evaluated(value) => visit(value),
            BackendAttributeValue::Failed(_) => {}
        }
    }
    fn visit_source_mut<P: ValueKind, B: BindingAddress>(
        _source: &mut BackendSource,
        _visit: &mut impl FnMut(&mut Value<P, B>),
    ) {
    }
}

pub enum BackendAdmissionInput<'a, P: ValueKind, B: BindingAddress> {
    Evaluated(&'a Value<P, B>),
    Failed(&'a Arc<ParseError>),
}
/// Authored backend inputs cannot enter schema admission.
pub trait BackendAdmissionPhase: BackendPhase {
    fn admission_input<P: ValueKind, B: BindingAddress>(
        attribute: &Self::Attribute<P, B>,
    ) -> BackendAdmissionInput<'_, P, B>;
}
impl BackendAdmissionPhase for StrictBackend {
    fn admission_input<P: ValueKind, B: BindingAddress>(
        attribute: &Value<P, B>,
    ) -> BackendAdmissionInput<'_, P, B> {
        BackendAdmissionInput::Evaluated(attribute)
    }
}
impl BackendAdmissionPhase for RetainedBackend {
    fn admission_input<P: ValueKind, B: BindingAddress>(
        attribute: &BackendAttributeValue<P, B>,
    ) -> BackendAdmissionInput<'_, P, B> {
        match attribute {
            BackendAttributeValue::Evaluated(value) => BackendAdmissionInput::Evaluated(value),
            BackendAttributeValue::Failed(cause) => BackendAdmissionInput::Failed(cause),
        }
    }
}

/// Test fixtures enter runtime APIs through the same evaluator as authored files.
#[cfg(any(test, feature = "test-utils"))]
pub trait BackendFixturePhase: BackendPhase {
    fn evaluate_fixture<E>(
        file: super::File<
            E,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            super::ParsedDeferredForPhase,
            crate::resource::RelativeBindingName,
            Self,
        >,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<super::File<E>, ParseError>;
}
#[cfg(any(test, feature = "test-utils"))]
impl BackendFixturePhase for AuthoredBackend {
    fn evaluate_fixture<E>(
        file: super::File<
            E,
            crate::resource::ParserPlaceholder,
            crate::resource::ParserPlaceholder,
            super::ParsedDeferredForPhase,
            crate::resource::RelativeBindingName,
            Self,
        >,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<super::File<E>, ParseError> {
        super::evaluate_file_backend::<StrictBackend, _>(
            file,
            session.catalog(),
            Some(session),
            crate::config_loader::strict_resolution_context(),
        )
    }
}
#[cfg(any(test, feature = "test-utils"))]
impl BackendFixturePhase for StrictBackend {
    fn evaluate_fixture<E>(
        file: super::File<E>,
        _session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<super::File<E>, ParseError> {
        Ok(file)
    }
}
#[cfg(any(test, feature = "test-utils"))]
pub fn evaluate_backend_for_test<E, K: BackendFixturePhase>(
    file: super::File<
        E,
        crate::resource::ParserPlaceholder,
        crate::resource::ParserPlaceholder,
        super::ParsedDeferredForPhase,
        crate::resource::RelativeBindingName,
        K,
    >,
    session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<super::File<E>, ParseError> {
    K::evaluate_fixture(file, session)
}
