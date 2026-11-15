//! Explicit parity-tree emission, shared by the example and the sandboxed parity test.
use super::round5_fixtures::*;
use std::path::Path;

impl Evaluation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::DiscardedIf => "discarded-if",
            Self::DiscardedCoalesce => "discarded-coalesce",
            Self::NeverExpanded => "never-expanded",
        }
    }
}

/// Emit twins for the external main-versus-branch parity runner.
pub fn emit_twins(directory: &Path) -> usize {
    let mut count = 0;
    let mut manifest = Vec::new();
    for context in CONTEXTS {
        for evaluation in EVALUATIONS {
            if let Ok(fixture) = context.fixture(evaluation, false) {
                fixture.write(
                    &directory
                        .join("placement")
                        .join(context.label)
                        .join(evaluation.label()),
                );
                manifest.push(format!(
                    "placement/{}/{}, {:?}: {:?}",
                    context.label,
                    evaluation.label(),
                    context.placement,
                    (
                        context.rules,
                        context.operand,
                        context.parser_rejection(evaluation)
                    )
                ));
                count += 1;
            } else {
                manifest.push(format!(
                    "excluded placement/{}/{}: declaration cannot occur in structural body",
                    context.label,
                    evaluation.label()
                ));
            }
        }
    }
    for position in POSITIONS {
        for known in [true, false] {
            for invalid in [false, true] {
                fixture(position, known, invalid, false).write(
                    &directory
                        .join("transport")
                        .join(position.label())
                        .join(if known { "known" } else { "unknown" })
                        .join(if invalid { "invalid" } else { "valid" }),
                );
                count += 1;
            }
        }
    }
    for position in OWNER_POSITIONS {
        position
            .fixture(false)
            .write(&directory.join("static-owner").join(position.label()));
        count += 1;
    }
    std::fs::write(directory.join("grammar-audit.txt"), format!("{}\nnon-expression rules: {NON_CONTEXT_RULES:?}\ntransport roles: {TRANSPORT_EXAMPLES:?}\n", manifest.join("\n"))).unwrap();
    count
}
