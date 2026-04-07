use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;

use carina_provider_resolver;

pub fn run_init(path: &Path, upgrade: bool) -> Result<(), String> {
    let base_dir = get_base_dir(path);
    let path_buf = path.to_path_buf();

    let provider_context = ProviderContext::default();
    let loaded = load_configuration_with_config(&path_buf, &provider_context)
        .map_err(|e| format!("Failed to load configuration: {e}"))?;

    let github_providers: Vec<_> = loaded
        .parsed
        .providers
        .iter()
        .filter(|p| p.source.as_ref().is_some_and(|s| !s.starts_with("file://")))
        .collect();

    if github_providers.is_empty() {
        println!(
            "{}",
            "No providers with remote source found. Nothing to do.".cyan()
        );
        return Ok(());
    }

    let action = if upgrade { "Upgrading" } else { "Resolving" };
    println!(
        "{}",
        format!("{} {} provider(s)...", action, github_providers.len()).cyan()
    );

    let resolved =
        carina_provider_resolver::resolve_all(base_dir, &loaded.parsed.providers, upgrade)?;

    println!(
        "{}",
        format!(
            "Done. {} provider(s) installed in .carina/providers/",
            resolved.len()
        )
        .green()
    );

    Ok(())
}
