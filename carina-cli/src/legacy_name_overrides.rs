use std::io::{self, Write};

use crate::wiring::{OverrideAwareResources, ProjectedStateRead};
use carina_core::name_override::NameOverrideSource;

pub(crate) fn emit_legacy_override_warnings(
    resources: &OverrideAwareResources,
    state: &ProjectedStateRead<'_>,
) {
    let mut stderr = io::stderr().lock();
    let _ = write_legacy_override_warnings(resources, state, &mut stderr);
}

pub(crate) fn write_legacy_override_warnings<W: Write>(
    resources: &OverrideAwareResources,
    state: &ProjectedStateRead<'_>,
    writer: &mut W,
) -> io::Result<()> {
    let mut warnings = Vec::new();
    for resource_id in resources.legacy_overrides_applied() {
        let Some(overrides) = state.name_overrides_for(resource_id) else {
            continue;
        };
        for (attribute, override_) in overrides {
            if override_.original_value.is_none() {
                warnings.push((
                    resource_id.to_string(),
                    attribute.clone(),
                    override_.temp_value.clone(),
                ));
            }
        }
    }
    warnings.sort();

    for (resource_id, attribute, temp_value) in warnings {
        writeln!(
            writer,
            "warning: applied legacy name override for {resource_id}:\n\
                     override attribute '{attribute}' set to '{temp_value}'; original DSL\n\
                     value was not recorded in pre-Phase-5 state. After this apply,\n\
                     the override will be tracked with full provenance."
        )?;
    }

    Ok(())
}
