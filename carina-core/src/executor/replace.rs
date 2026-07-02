//! Shared replacement/update patch helpers.

use std::collections::HashMap;
use std::time::Duration;

use crate::differ::{
    AttrComparison, TypedAttr, key_should_enter_patch, secret_grafted_comparison_view,
};
use crate::provider::{PatchOp, PatchOpKind, UpdatePatch, build_update_patch};
use crate::resource::{DataSource, ResolvedResource, Resource, ResourceId, State, Value};
use crate::schema::SchemaRegistry;
use crate::value::SecretHashContext;

use super::ProgressInfo;
use super::basic::BasicEffectResult;
use super::wait::WaitOutcome;

/// Build a full attribute-diff [`UpdatePatch`] between an existing
/// `from` state and a desired `to` resource when no precomputed
/// `changed_attributes` list is available.
pub fn compute_full_diff_patch(
    from: &State,
    to: &ResolvedResource,
    to_source: &Resource,
    schemas: &SchemaRegistry,
    resource_id: &ResourceId,
) -> UpdatePatch {
    use std::collections::HashSet;

    let to_resource = to.as_resource();
    let schema = schemas.get_for(to_resource);
    let mut keys: HashSet<&str> = HashSet::new();
    keys.extend(from.attributes.keys().map(String::as_str));
    keys.extend(to_resource.attributes.keys().map(String::as_str));
    let mut sorted_keys: Vec<&str> = keys.into_iter().collect();
    sorted_keys.sort();

    let changed: Vec<String> = sorted_keys
        .into_iter()
        .filter_map(|key| match to_resource.attributes.get(key) {
            Some(new_value) => {
                let type_info = schema.and_then(|s| {
                    s.attributes.get(key).map(|attr| TypedAttr {
                        attr_type: &attr.attr_type,
                        defs: &s.defs,
                    })
                });
                let secret_ctx = Some(SecretHashContext::new(
                    resource_id.display_type(),
                    resource_id.identity_or_empty(),
                    key,
                ));
                let comparison_value =
                    secret_grafted_comparison_view(new_value, to_source.attributes.get(key))?;
                key_should_enter_patch(
                    key,
                    schema,
                    AttrComparison {
                        from: from.attributes.get(key),
                        to: comparison_value.as_ref(),
                        saved: None,
                        type_info,
                        secret_ctx: secret_ctx.as_ref(),
                    },
                )
                .then(|| key.to_string())
            }
            None => (!key.starts_with('_')).then(|| key.to_string()),
        })
        .collect();
    build_update_patch(&changed, to, from)
}

/// Build a single-attribute [`UpdatePatch`] when exactly one
/// attribute should be patched.
#[allow(dead_code)]
pub(super) fn single_attribute_patch(key: String, value: Value) -> UpdatePatch {
    UpdatePatch {
        ops: vec![PatchOp {
            kind: PatchOpKind::Replace,
            key,
            value: Some(value),
        }],
    }
}

/// Result of executing a single effect.
pub(super) enum SingleEffectResult {
    /// Create/Update/Delete completed (wraps BasicEffectResult)
    Basic(BasicEffectResult),
    ReadNoOp,
    /// Apply-time data-source read outcome. Successful reads publish
    /// their returned state under the data-source binding before
    /// downstream effects are scheduled.
    Read {
        resource: Box<DataSource>,
        resolved_attrs: HashMap<String, Value>,
        outcome: Result<State, String>,
        duration: Duration,
        progress: ProgressInfo,
    },
    /// `Effect::Wait` execution outcome. On success carries the
    /// captured target state so the parallel scheduler can register it
    /// under the wait binding for downstream resolution. On failure
    /// carries the wait binding so dependents can be marked failed.
    Wait {
        binding: String,
        outcome: WaitOutcome,
        duration: Duration,
        progress: ProgressInfo,
    },
}
