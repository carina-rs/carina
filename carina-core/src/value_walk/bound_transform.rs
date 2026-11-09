//! Bind the public, stage-polymorphic algebra to one input stage. Parser
//! substitution policies are monomorphic; both adapters use the same dispatcher
//! and typed Parts fold. This interface is private to the kernel.

use super::*;

pub(super) trait TransformOutput {
    type Output: WalkOutput;
}

pub(super) trait BoundTransform<P: ValueKind, B: BindingAddress>: TransformOutput {
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
        functions: Arc<HashMap<String, crate::parser::UserFunction<B>>>,
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
    /// The engine inspects every original operand before accepting a replacement.
    /// Inserted arguments have the same nonrecursive semantics as leaf replacement.
    fn argument(
        &mut self,
        _at: &At<'_>,
        _argument: &FunctionArgument<P, B>,
    ) -> Result<Option<FunctionArgument<P, B>>, WalkError> {
        Ok(None)
    }
}

pub(super) struct PublicTransform<'a, A>(pub(super) &'a mut A);

impl<A: Algebra<Transform>> TransformOutput for PublicTransform<'_, A> {
    type Output = A::Output;
}

impl<P: ValueKind, B: BindingAddress, A: Algebra<Transform>> BoundTransform<P, B>
    for PublicTransform<'_, A>
{
    fn scalar(
        &mut self,
        at: &At<'_>,
        value: ScopedScalar<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        self.0.scalar(at, value)
    }
    fn list(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        self.0.list(at, children)
    }
    fn map(
        &mut self,
        at: &At<'_>,
        children: Children<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        self.0.map(at, children)
    }
    fn expression(
        &mut self,
        at: &At<'_>,
        expression: ExprChildren<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        self.0.expression(at, expression)
    }
    fn placeholder(
        &mut self,
        at: &At<'_>,
        leaf: ScopedLeaf<'_, Transform, P, B>,
    ) -> Step<Transform, Self::Output, P, B> {
        self.0.placeholder(at, leaf)
    }
    fn secret(
        &mut self,
        at: &SecretAt<'_>,
        child: SecretChild<'_, Transform, P, B>,
    ) -> SecretStep<Transform, Self::Output, P, B> {
        self.0.secret(at, child)
    }
}
