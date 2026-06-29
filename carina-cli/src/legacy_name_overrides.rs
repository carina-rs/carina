use std::io::{self, Write};

use carina_core::override_aware::{NameOverrideSource, OverrideAwareResources};
use carina_state::StateFile;

pub(crate) fn emit_legacy_override_warnings(
    resources: &OverrideAwareResources,
    state_file: Option<&StateFile>,
) {
    let mut stderr = io::stderr().lock();
    let _ = write_legacy_override_warnings(resources, state_file, &mut stderr);
}

pub(crate) fn write_legacy_override_warnings<W: Write>(
    resources: &OverrideAwareResources,
    state_file: Option<&StateFile>,
    writer: &mut W,
) -> io::Result<()> {
    let Some(state_file) = state_file else {
        return Ok(());
    };

    let mut warnings = Vec::new();
    for resource_id in resources.legacy_overrides_applied() {
        let Some(overrides) = state_file.name_overrides_for(resource_id) else {
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
