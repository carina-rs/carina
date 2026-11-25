//! One binding conversion owns the callback and all definition mappings.
//! Recursive value conversion cannot create a fresh memo accidentally.
use super::*;
use crate::parser::{FnParam, UserFunction, UserFunctionBody};

type Functions<B> = IndexMap<String, UserFunction<B>>;
type Mapped<S, T> = HashMap<usize, (Arc<S>, Arc<T>)>;

pub(crate) struct BindingMapper<'a, B: BindingAddress, T: BindingAddress> {
    map: &'a mut dyn FnMut(B) -> T,
    definitions: Mapped<UserFunction<B>, UserFunction<T>>,
    scopes: Mapped<Functions<B>, Functions<T>>,
}

impl<'a, B: BindingAddress, T: BindingAddress> BindingMapper<'a, B, T> {
    pub(crate) fn new(map: &'a mut impl FnMut(B) -> T) -> Self {
        Self {
            map,
            definitions: HashMap::new(),
            scopes: HashMap::new(),
        }
    }

    pub(crate) fn binding(&mut self, binding: B) -> T {
        (self.map)(binding)
    }

    pub(super) fn definition(&mut self, source: Arc<UserFunction<B>>) -> Arc<UserFunction<T>> {
        let key = Arc::as_ptr(&source) as usize;
        #[cfg(test)]
        let bypass = crate::function_memo_test::bypassed();
        #[cfg(not(test))]
        let bypass = false;
        if !bypass && let Some((_, mapped)) = self.definitions.get(&key) {
            let mapped = Arc::clone(mapped);
            #[cfg(test)]
            if crate::function_memo_test::enabled() {
                crate::function_memo_test::binding_hit();
                let _reference = crate::function_memo_test::ReferenceWalk::enter();
                let checked = self.function_body((*source).clone());
                assert!(
                    super::storage::same_function(&mapped, &checked),
                    "binding memo definition mismatch"
                );
            }
            return mapped;
        }
        let mapped = Arc::new(self.function_body((*source).clone()));
        if !bypass {
            // Retain the source allocation until this conversion ends. Pointer
            // identity must never alias a later allocation or in-place edit.
            self.definitions.insert(key, (source, Arc::clone(&mapped)));
        }
        mapped
    }

    pub(super) fn scope(&mut self, source: Arc<Functions<B>>) -> Arc<Functions<T>> {
        let key = Arc::as_ptr(&source) as usize;
        #[cfg(test)]
        let bypass = crate::function_memo_test::bypassed();
        #[cfg(not(test))]
        let bypass = false;
        if !bypass && let Some((_, mapped)) = self.scopes.get(&key) {
            let mapped = Arc::clone(mapped);
            #[cfg(test)]
            if crate::function_memo_test::enabled() {
                crate::function_memo_test::binding_hit();
                let _reference = crate::function_memo_test::ReferenceWalk::enter();
                let checked = self.scope_body(&source);
                assert_eq!(mapped.len(), checked.len());
                for ((name, cached), (checked_name, checked)) in mapped.iter().zip(&checked) {
                    assert_eq!(name, checked_name);
                    assert!(
                        super::storage::same_function(cached, checked),
                        "binding memo scope mismatch"
                    );
                }
            }
            return mapped;
        }
        let mapped = Arc::new(self.scope_body(&source));
        if !bypass {
            self.scopes.insert(key, (source, Arc::clone(&mapped)));
        }
        mapped
    }

    fn scope_body(&mut self, source: &Functions<B>) -> Functions<T> {
        source
            .iter()
            .map(|(name, function)| (name.clone(), self.function_body(function.clone())))
            .collect()
    }

    // This is the only binding-conversion entry into a definition body, and
    // the mapper is mandatory even for owned top-level definitions.
    pub(crate) fn function_body(&mut self, source: UserFunction<B>) -> UserFunction<T> {
        #[cfg(test)]
        crate::parser::USER_FUNCTION_BINDING_MAP_COUNT.with(|count| count.set(count.get() + 1));
        let UserFunction {
            type_annotations,
            name,
            params,
            return_type,
            return_type_span,
            annotation_source_file,
            local_lets,
            body,
        } = source;
        UserFunction {
            type_annotations,
            name,
            params: params
                .into_iter()
                .map(|param| FnParam {
                    name: param.name,
                    param_type: param.param_type,
                    type_span: param.type_span,
                    default: param.default.map(|value| value.map_bindings_in(self)),
                })
                .collect(),
            return_type,
            return_type_span,
            annotation_source_file,
            local_lets: local_lets
                .into_iter()
                .map(|(name, value, line)| (name, value.map_bindings_in(self), line))
                .collect(),
            body: UserFunctionBody {
                location: body.location,
                value: body.value.map_bindings_in(self),
                source_line: body.source_line,
                source_file: body.source_file,
            },
        }
    }
}
