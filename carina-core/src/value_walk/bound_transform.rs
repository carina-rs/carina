//! Internal Transform operations for one input stage. Parser substitution
//! policies use the owned dispatcher and typed Parts fold.

use super::*;

pub(super) trait TransformOutput {
    type Output: WalkOutput;
}

pub(super) trait BoundTransform<P: ValueKind, B: BindingAddress>: TransformOutput {
    fn function_memo(&self) -> Option<engine::DefinitionMemo<B, Self::Output>>;

    fn scalar(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B>;
    fn list(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B>;
    fn map(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B>;
    fn expression(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B>;
    fn placeholder(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B>;
    fn secret(
        &mut self,
        at: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, Self::Output, P, B>;

    fn call(
        &mut self,
        _at: &At<'_>,
        name: crate::builtins::BuiltinFunction,
        args: Vec<FunctionArgument<P, B>>,
        source_line: crate::resource::DiagnosticSourceLine,
    ) -> Result<Value<P, B>, WalkError> {
        Ok(Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }))
    }
    fn resolved_call(
        &mut self,
        _at: &At<'_>,
        function: Arc<crate::parser::UserFunction<B>>,
        functions: Arc<indexmap::IndexMap<String, crate::parser::UserFunction<B>>>,
        args: Vec<FunctionArgument<P, B>>,
        source_line: crate::resource::DiagnosticSourceLine,
    ) -> Result<Value<P, B>, WalkError> {
        Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }))
    }

    /// Whole-argument replacement retains structural handles and their metadata.
    /// Inserted arguments have the same nonrecursive semantics as leaf replacement.
    fn argument(
        &mut self,
        _at: &At<'_>,
        _argument: &FunctionArgument<P, B>,
    ) -> Result<Option<FunctionArgument<P, B>>, WalkError> {
        Ok(None)
    }
}
