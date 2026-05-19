use std::path::Path;

use colored::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;
use carina_state::BackendLock;

use carina_provider_resolver::{self, LockMode};

use crate::commands::migrate_state::{MigrationOutcome, SourceDisposition, run_init_migrate_state};

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

    // Backend lock lifecycle:
    //
    // - No lock yet            → first init, create it (nothing to migrate).
    // - Lock matches config    → nothing to do.
    // - Lock differs + no flag → refuse, point at --migrate-state.
    // - Lock differs + flag    → migrate state, then re-lock.
    let existing_lock =
        BackendLock::load(base_dir).map_err(|e| format!("Failed to read backend lock: {e}"))?;
    let configured = BackendLock::for_config(backend_config)
        .map_err(|e| format!("Invalid backend configuration: {e}"))?;

    match existing_lock {
        None => {
            crate::commands::ensure_backend_lock(base_dir, backend_config)
                .map_err(|e| format!("Failed to create backend lock: {e}"))?;
        }
        Some(existing) if existing == configured => {
            // Already initialized against this backend; nothing to do.
        }
        Some(existing) => {
            if !migrate_state {
                return Err(format!(
                    "Backend configuration changed since the last init:\n\n{}\n\n\
                     Re-run with `--migrate-state` to move the state file from the \
                     old backend to the configured one, or revert the backend \
                     configuration to match the lock.",
                    existing.describe_diff(&configured)
                ));
            }
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

    println!("{}", "Initialized successfully.".green());

    Ok(())
}
