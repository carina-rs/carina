use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;

use carina_provider_resolver::{self, LockMode};

pub fn run_init(path: &Path, upgrade: bool, locked: bool) -> Result<(), String> {
    if upgrade && locked {
        return Err("--upgrade and --locked are mutually exclusive".to_string());
    }
    let mode = if upgrade {
        LockMode::Upgrade
    } else if locked {
        LockMode::Locked
    } else {
        LockMode::Normal
    };

    let base_dir = get_base_dir(path);
    let path_buf = path.to_path_buf();

    let provider_context = ProviderContext::default();
    let loaded = load_configuration_with_config(
        &path_buf,
        &provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(|e| format!("Failed to load configuration: {e}"))?;

    let missing_source: Vec<String> = loaded
        .parsed
        .providers
        .iter()
        .filter(|p| p.source.is_none())
        .map(|p| crate::commands::missing_provider_source_message(&p.name))
        .collect();
    if !missing_source.is_empty() {
        return Err(missing_source.join("\n"));
    }

    if !loaded.parsed.providers.is_empty() {
        let action = match mode {
            LockMode::Upgrade => "Upgrading",
            LockMode::Locked => "Verifying locked",
            LockMode::Normal => "Resolving",
        };
        println!(
            "{}",
            format!(
                "{} {} provider(s)...",
                action,
                loaded.parsed.providers.len()
            )
            .cyan()
        );

        let resolved =
            carina_provider_resolver::resolve_all(base_dir, &loaded.parsed.providers, mode)?;

        println!(
            "{}",
            format!(
                "{} provider(s) installed in .carina/providers/",
                resolved.len()
            )
            .green()
        );
    }

    // Create backend lock so apply/destroy can detect backend config changes
    crate::commands::ensure_backend_lock(base_dir, loaded.parsed.backend.as_ref())
        .map_err(|e| format!("Failed to create backend lock: {e}"))?;

    println!("{}", "Initialized successfully.".green());

    Ok(())
}
