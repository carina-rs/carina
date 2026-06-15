use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{DeferredForExpression, expand_deferred_children};

pub(super) fn expand_deferred_for_effects(
    upstream_binding: &str,
    template: &DeferredForExpression,
    bindings: &ResolvedBindings,
) -> Vec<Effect> {
    let upstream_attrs = bindings.get(upstream_binding).unwrap_or_else(|| {
        panic!(
            "ExpandDeferredFor upstream binding `{}` was not published before dispatch",
            upstream_binding
        )
    });
    let iterable = upstream_attrs
        .get(&template.iterable_attr)
        .unwrap_or_else(|| {
            panic!(
                "ExpandDeferredFor upstream binding `{}` does not contain iterable attribute `{}`",
                upstream_binding, template.iterable_attr
            )
        });

    expand_deferred_children(template, iterable)
        .into_iter()
        .map(Effect::Create)
        .collect()
}
