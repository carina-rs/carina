pub mod commands;
pub mod display;
pub mod error;
pub mod fixture_plan;
pub mod signal;
pub mod wiring;

#[cfg(test)]
mod module_info_snapshot_tests;
#[cfg(test)]
mod module_list_tests;
#[cfg(test)]
mod plan_snapshot_tests;
#[cfg(test)]
mod tests;

use clap::ValueEnum;

/// Controls how much detail is shown in plan output (CLI-facing enum with clap support).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DetailLevel {
    /// Show all attributes: user-specified, defaults, read-only, and unchanged (dimmed)
    Full,
    /// Show only attributes explicitly specified in .crn file
    Explicit,
    /// Show resource names only (no attributes)
    None,
}

impl DetailLevel {
    /// Convert to the core `DetailLevel` enum used by `build_detail_rows`.
    pub fn to_core(self) -> carina_core::detail_rows::DetailLevel {
        match self {
            DetailLevel::Full => carina_core::detail_rows::DetailLevel::Full,
            DetailLevel::Explicit => carina_core::detail_rows::DetailLevel::Explicit,
            DetailLevel::None => carina_core::detail_rows::DetailLevel::NamesOnly,
        }
    }
}
