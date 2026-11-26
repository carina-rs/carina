//! Definition traversal has one entry: it always applies the definition edge
//! and the adapter's memo policy before entering a body.
use super::*;
use std::cell::RefCell;

pub(super) enum FunctionSource<B: BindingAddress> {
    Definition(Arc<UserFunction<B>>),
    Scope {
        functions: Arc<indexmap::IndexMap<String, UserFunction<B>>>,
        name: String,
        index: usize,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum FunctionKey {
    Definition(usize),
    Scope(usize),
}

impl<B: BindingAddress> FunctionSource<B> {
    fn function(&self) -> &UserFunction<B> {
        match self {
            Self::Definition(function) => function,
            Self::Scope {
                functions,
                name,
                index: _,
            } => &functions[name],
        }
    }
    fn key(&self) -> FunctionKey {
        match self {
            Self::Definition(function) => FunctionKey::Definition(Arc::as_ptr(function) as usize),
            Self::Scope { .. } => {
                FunctionKey::Scope(self.function() as *const UserFunction<B> as usize)
            }
        }
    }
    fn edge(&self) -> ExpressionEdge {
        match self {
            Self::Definition(_) => ExpressionEdge::FunctionDefinition,
            Self::Scope { index, .. } => ExpressionEdge::FunctionScope(*index),
        }
    }
}

struct FunctionCandidate<B: BindingAddress, O: WalkOutput> {
    candidate: Option<UserFunction<B>>,
    output: Vec<O>,
    changed: bool,
    encountered_secret: bool,
}

impl<B: BindingAddress, O: WalkOutput> FunctionCandidate<B, O> {
    fn copied(&self, copy_output: fn(&O) -> O) -> Self {
        Self {
            candidate: self.candidate.clone(),
            output: self.output.iter().map(copy_output).collect(),
            changed: self.changed,
            encountered_secret: self.encountered_secret,
        }
    }
}

struct Memo<B: BindingAddress, O: WalkOutput> {
    entries: HashMap<FunctionKey, (FunctionSource<B>, FunctionCandidate<B, O>)>,
    copy_output: fn(&O) -> O,
    #[cfg(test)]
    debug_output: fn(&[O]) -> String,
}

/// Shared across rewrites within an evaluation. Only structural unit-output
/// walks can construct a memo; arbitrary public transforms cannot opt in.
/// Source Arcs keep definition and table-entry addresses stable until it drops.
pub(crate) struct DefinitionMemo<B: BindingAddress, O: WalkOutput = ()>(Rc<RefCell<Memo<B, O>>>);

impl<B: BindingAddress, O: WalkOutput> Clone for DefinitionMemo<B, O> {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}
impl<B: BindingAddress> Default for DefinitionMemo<B> {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(Memo {
            entries: HashMap::new(),
            copy_output: |output| *output,
            #[cfg(test)]
            debug_output: |output| format!("{output:?}"),
        })))
    }
}

impl<O: WalkOutput> TransformFold<O> {
    pub(super) fn function_candidate<
        B: BindingAddress,
        A: BoundTransform<crate::resource::ParserPlaceholder, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        source: FunctionSource<B>,
        at: &At<'_>,
        algebra: &mut A,
    ) -> Result<Option<UserFunction<B>>, WalkError> {
        let at = at.operand(source.edge());
        let key = source.key();
        let memo = algebra.function_memo();
        #[cfg(test)]
        let bypass = crate::function_memo_test::bypassed();
        #[cfg(not(test))]
        let bypass = false;
        let hit = if bypass {
            None
        } else {
            memo.as_ref().and_then(|memo| {
                let memo = memo.0.borrow();
                memo.entries
                    .get(&key)
                    .map(|(_, candidate)| candidate.copied(memo.copy_output))
            })
        };
        if let Some(hit) = hit {
            #[cfg(test)]
            if crate::function_memo_test::enabled() {
                crate::function_memo_test::transform_hit();
                let _reference = crate::function_memo_test::ReferenceWalk::enter();
                let checked =
                    Self::walk_function_candidate(source.function(), at.clone(), algebra)?;
                assert_eq!(
                    hit.candidate.is_some(),
                    checked.candidate.is_some(),
                    "transform memo candidate presence"
                );
                assert!(
                    crate::resource::storage::same_function(
                        hit.candidate.as_ref().unwrap_or(source.function()),
                        checked.candidate.as_ref().unwrap_or(source.function()),
                    ),
                    "transform memo candidate mismatch"
                );
                if let Some(memo) = &memo {
                    let debug_output = memo.0.borrow().debug_output;
                    assert_eq!(
                        debug_output(&hit.output),
                        debug_output(&checked.output),
                        "transform memo output"
                    );
                }
                assert_eq!(hit.changed, checked.changed, "transform memo changed flag");
                assert_eq!(
                    hit.encountered_secret, checked.encountered_secret,
                    "transform memo secret discovery"
                );
            }
            if hit.encountered_secret {
                at.encountered_secret.set(true);
            }
            self.output.extend(hit.output);
            self.changed |= hit.changed;
            return Ok(hit.candidate);
        }
        let candidate = Self::walk_function_candidate(source.function(), at, algebra)?;
        if !bypass && let Some(memo) = memo {
            let mut memo = memo.0.borrow_mut();
            let copied = candidate.copied(memo.copy_output);
            memo.entries.insert(key, (source, copied));
        }
        self.output.extend(candidate.output);
        self.changed |= candidate.changed;
        Ok(candidate.candidate)
    }

    fn walk_function_candidate<
        B: BindingAddress,
        A: BoundTransform<crate::resource::ParserPlaceholder, B> + TransformOutput<Output = O>,
    >(
        function: &UserFunction<B>,
        mut at: At<'_>,
        algebra: &mut A,
    ) -> Result<FunctionCandidate<B, O>, WalkError> {
        #[cfg(test)]
        crate::value_walk::FUNCTION_DEFINITION_WALK_COUNT.with(|count| count.set(count.get() + 1));
        let parent_secret = at.encountered_secret.clone();
        let encountered_secret = Rc::new(Cell::new(false));
        at.encountered_secret = encountered_secret.clone();
        let mut nested = Self::new();
        let candidate = nested.function(function.clone(), at, algebra);
        // Discovery propagates on failure too, so errors retain protection.
        if encountered_secret.get() {
            parent_secret.set(true);
        }
        let candidate = candidate?;
        Ok(FunctionCandidate {
            candidate: nested.changed.then_some(candidate),
            output: nested.output,
            changed: nested.changed,
            encountered_secret: encountered_secret.get(),
        })
    }
    fn function<
        B: BindingAddress,
        A: BoundTransform<crate::resource::ParserPlaceholder, B> + TransformOutput<Output = O>,
    >(
        &mut self,
        mut function: UserFunction<B>,
        at: At<'_>,
        algebra: &mut A,
    ) -> Result<UserFunction<B>, WalkError> {
        for (index, param) in function.params.iter_mut().enumerate() {
            if let Some(value) = param.default.take() {
                param.default = Some(self.value(
                    value,
                    at.operand(ExpressionEdge::FunctionDefault(index)),
                    algebra,
                )?);
            }
        }
        function.local_lets = function
            .local_lets
            .into_iter()
            .enumerate()
            .map(|(index, (name, value, line))| {
                self.value(
                    value,
                    at.operand(ExpressionEdge::FunctionLocal(index)),
                    algebra,
                )
                .map(|value| (name, value, line))
            })
            .collect::<Result<_, _>>()?;
        function.body.value = self.value(
            function.body.value,
            at.operand(ExpressionEdge::FunctionBody),
            algebra,
        )?;
        Ok(function)
    }
}
