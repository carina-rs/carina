use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::{DeferredForExpression, ShapeMismatch, expand_deferred_children};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExpansionFailure {
    UpstreamBindingMissing {
        upstream_binding: String,
    },
    IterableAttrMissing {
        upstream_binding: String,
        attr: String,
    },
    ShapeMismatch {
        upstream_binding: String,
        attr: String,
        mismatch: ShapeMismatch,
    },
}

impl ExpansionFailure {
    pub(super) fn message(&self) -> String {
        match self {
            ExpansionFailure::UpstreamBindingMissing { upstream_binding } => {
                format!(
                    "deferred-for expansion upstream binding `{upstream_binding}` was not published before dispatch"
                )
            }
            ExpansionFailure::IterableAttrMissing {
                upstream_binding,
                attr,
            } => {
                format!(
                    "deferred-for expansion upstream binding `{upstream_binding}` does not contain iterable attribute `{attr}`"
                )
            }
            ExpansionFailure::ShapeMismatch {
                upstream_binding,
                attr,
                mismatch,
            } => {
                format!(
                    "deferred-for expansion expected {} for `{upstream_binding}.{attr}` but got {}",
                    mismatch.expected_kind(),
                    mismatch.got_kind()
                )
            }
        }
    }
}

pub(super) fn expand_deferred_for_effects(
    upstream_binding: &str,
    template: &DeferredForExpression,
    bindings: &ResolvedBindings,
) -> Result<Vec<Effect>, ExpansionFailure> {
    let upstream_attrs =
        bindings
            .get(upstream_binding)
            .ok_or_else(|| ExpansionFailure::UpstreamBindingMissing {
                upstream_binding: upstream_binding.to_string(),
            })?;
    let iterable = upstream_attrs.get(&template.iterable_attr).ok_or_else(|| {
        ExpansionFailure::IterableAttrMissing {
            upstream_binding: upstream_binding.to_string(),
            attr: template.iterable_attr.clone(),
        }
    })?;

    Ok(expand_deferred_children(template, iterable)
        .map_err(|mismatch| ExpansionFailure::ShapeMismatch {
            upstream_binding: upstream_binding.to_string(),
            attr: template.iterable_attr.clone(),
            mismatch,
        })?
        .into_iter()
        .map(Effect::Create)
        .collect())
}
