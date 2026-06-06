use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;

use carina_provider_resolver::{self, LockMode};

use crate::commands::migrate_state::{MigrationOutcome, SourceDisposition, run_init_migrate_state};
use crate::commands::{
    BackendDriftStatus, drift_warning, ensure_backend_lock, inspect_backend_drift,
};

pub async fn run_init(
    path: &Path,
    upgrade: bool,
    locked: bool,
    migrate_state: bool,
    force: bool,
) -> Result<(), String> {
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

    // `source` is a kind-level property — only the kind's default
    // instance carries it. Named instances declared as
    // `let <name> = provider <kind> { ... }` cannot set it (the
    // parser rejects them). Restrict the pre-resolution check to
    // default instances so a named instance's deliberate absence
    // of `source` does not surface as a user error (carina#3023).
    let missing_source: Vec<String> = loaded
        .parsed
        .providers
        .iter()
        .filter(|p| p.is_default && p.source.is_none())
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

    let backend_config = loaded.parsed.backend.as_ref();
    let mut migration_pending = false;

    match inspect_backend_drift(base_dir, backend_config).map_err(|e| e.to_string())? {
        BackendDriftStatus::Fresh => {
            ensure_backend_lock(base_dir, backend_config)
                .map_err(|e| format!("Failed to create backend lock: {e}"))?;
            if migrate_state {
                println!(
                    "{}",
                    "No state migration needed; backend lock initialized.".green()
                );
            }
        }
        BackendDriftStatus::Unchanged => {
            // Already initialized against this backend; nothing to do.
            if migrate_state {
                println!(
                    "{}",
                    "No state migration needed; backend lock already matches the configuration."
                        .green()
                );
            }
        }
        BackendDriftStatus::Drifted {
            existing,
            configured,
        } => {
            if !migrate_state {
                eprintln!("{}", drift_warning(&existing, &configured).yellow());
                migration_pending = true;
            } else {
                match run_init_migrate_state(base_dir, backend_config, force)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    MigrationOutcome::NotNeeded => {
                        // Races with another init that already migrated; the
                        // lock now matches, so this is a benign no-op.
                    }
                    MigrationOutcome::Migrated { resources, source } => {
                        println!(
                            "{}",
                            format!("State migrated ({resources} resource(s)).").green()
                        );
                        // `DeleteFailed` already produced a precise stderr
                        // warning inside the migration; only the deliberate
                        // remote-backup case needs this extra hint here.
                        if source == SourceDisposition::KeptAsBackup {
                            println!(
                                "The old state at the previous backend was kept as a \
                                 backup; remove it manually once you have confirmed \
                                 the new backend."
                            );
                        }
                    }
                }
            }
        }
    }

    if migration_pending {
        println!(
            "{}",
            "Backend migration pending. Provider plugins resolved; run `carina init --migrate-state .` to complete initialization."
                .yellow()
        );
    } else {
        println!("{}", "Initialized successfully.".green());
    }

    Ok(())
}
