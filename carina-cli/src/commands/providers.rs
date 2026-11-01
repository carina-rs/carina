use std::io::{self, Write};
use std::path::{Path, PathBuf};

use carina_provider_resolver::{
    DiscardedSignerPins, GuardedLockFile, LockFileMigration, ProviderInstallLock, WorkflowPins,
};

use crate::error::AppError;
use carina_core::hint::ProjectCommand;

#[derive(clap::Subcommand)]
pub enum ProvidersCommands {
    /// Replace a registry host's discovery pin on its next resolution
    RepinDiscovery {
        /// Resolved registry hostname
        host: String,

        /// Proceed without typing the registry host to confirm
        #[arg(long)]
        force: bool,

        /// Path to the directory containing carina-providers.lock
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Replace all of a registry provider's signer pins on its next resolution
    RepinIdentity {
        /// Registry provider source: [hostname/]namespace/name
        provider: String,

        /// Proceed without typing the provider source to confirm
        #[arg(long)]
        force: bool,

        /// Path to the directory containing carina-providers.lock
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Reset a registry provider's sequence observation, anchor and presence record
    #[command(name = "re-bootstrap")]
    ReBootstrap {
        /// Registry provider source: [hostname/]namespace/name
        provider: String,

        /// Proceed without typing the provider source to confirm
        #[arg(long)]
        force: bool,

        /// Path to the directory containing carina-providers.lock
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

pub fn run_providers_command(command: ProvidersCommands) -> Result<(), AppError> {
    match command {
        ProvidersCommands::RepinDiscovery { host, force, path } => {
            run_repin_discovery(&host, force, &path)
        }
        ProvidersCommands::RepinIdentity {
            provider,
            force,
            path,
        } => run_repin_identity(&provider, force, &path),
        ProvidersCommands::ReBootstrap {
            provider,
            force,
            path,
        } => run_rebootstrap(&provider, force, &path),
    }
}

fn acquire_install_lock(path: &Path) -> Result<ProviderInstallLock, AppError> {
    let root = carina_core::paths::ResolvedPath::new(path)
        .map_err(|error| AppError::Config(error.to_string()))?;
    if !root.is_dir()
        || !root
            .join("carina-providers.lock")
            .map_err(|error| AppError::Config(error.to_string()))?
            .exists()
    {
        return Err(missing_lock_error(&root));
    }
    ProviderInstallLock::acquire(&root).map_err(|error| {
        AppError::Config(format!(
            "Failed to acquire provider install lock for {}: {error}",
            root.display()
        ))
    })
}

fn missing_lock_error(base_dir: &carina_core::paths::ResolvedPath) -> AppError {
    AppError::Config(format!(
        "Provider lock file {} does not exist; run `{}` first",
        base_dir.display().join("carina-providers.lock"),
        ProjectCommand::for_display_path("init", base_dir.display())
    ))
}

fn load_lock<'lock>(
    install_lock: &'lock ProviderInstallLock,
) -> Result<GuardedLockFile<'lock>, AppError> {
    let loaded = install_lock
        .load_lock_file()
        .map_err(|error| AppError::Config(error.to_string()))?
        .ok_or_else(|| missing_lock_error(install_lock.project_dir()))?;
    Ok(loaded)
}

fn report_lock_migration(migration: Option<&LockFileMigration>) {
    if let Some(migration) = migration {
        eprintln!("{migration}");
    }
}

fn flush_preview() -> Result<(), AppError> {
    io::stderr()
        .flush()
        .map_err(|error| AppError::Config(format!("Failed to print recovery preview: {error}")))
}

fn save_lock(lock: &mut GuardedLockFile<'_>) -> Result<(), AppError> {
    let lock_path = lock.project_dir().display().join("carina-providers.lock");
    lock.save().map_err(|error| {
        AppError::Config(format!(
            "Failed to save provider lock {}: {error}",
            lock_path
        ))
    })
}

fn confirm_recovery(target: &str, target_label: &str, force: bool) -> Result<bool, AppError> {
    if force {
        return Ok(true);
    }

    println!();
    println!("Type the {target_label} to confirm recovery:");
    print!("  Enter {target_label}: ");
    io::stdout()
        .flush()
        .map_err(|error| AppError::Config(format!("Failed to print recovery prompt: {error}")))?;

    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|error| {
        AppError::Config(format!("Failed to read recovery confirmation: {error}"))
    })?;

    // Protocol §2 requires a contemporaneous operator instruction naming the
    // target; echoing it here makes that instruction explicit.
    if input.trim() != target {
        println!();
        println!("Recovery cancelled.");
        return Ok(false);
    }
    Ok(true)
}

fn run_repin_discovery(host: &str, force: bool, base_dir: &Path) -> Result<(), AppError> {
    let install_lock = acquire_install_lock(base_dir)?;
    // The recovery preview folds this explicit load event into its discard
    // report, so printing the event separately would duplicate its header.
    let mut lock = load_lock(&install_lock)?;
    let migration = lock.take_migration();
    let recovery = lock
        .prepare_registry_discovery_repin(host)
        .map_err(|error| AppError::Config(error.to_string()))?;
    eprintln!("{}", recovery.display_with_migration(migration.as_ref()));
    flush_preview()?;

    if !confirm_recovery(host, "registry host", force)? {
        return Ok(());
    }
    recovery.commit();
    save_lock(&mut lock)?;
    println!(
        "Registry discovery pin cleared. Run `{}` to verify discovery and acquire the new host pin.",
        ProjectCommand::new("init", base_dir)
    );
    Ok(())
}

fn run_repin_identity(provider: &str, force: bool, base_dir: &Path) -> Result<(), AppError> {
    let install_lock = acquire_install_lock(base_dir)?;
    let mut lock = load_lock(&install_lock)?;
    let migration = lock.take_migration();
    report_lock_migration(migration.as_ref());
    let recovery = lock
        .prepare_registry_identity_repin(provider)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let pin = recovery.identity_pin();

    eprintln!("Registry provider: {provider}");
    match pin {
        DiscardedSignerPins::Legacy(pin) => {
            eprintln!(
                "Discarding legacy certificate identity: {}",
                pin.certificate_san
            );
            eprintln!("Discarding legacy OIDC issuer: {}", pin.oidc_issuer);
        }
        DiscardedSignerPins::Current(pins) => {
            eprintln!("Discarding source OIDC issuer: {}", pins.source.oidc_issuer);
            eprintln!(
                "Discarding source repository owner ID: {}",
                pins.source.repository_owner_id
            );
            eprintln!(
                "Discarding source repository ID: {}",
                pins.source.repository_id
            );
            match &pins.workflows {
                WorkflowPins::Stable(workflow) => {
                    eprintln!("Discarding stable workflow: {}", workflow.0);
                }
                WorkflowPins::Prerelease(workflow) => {
                    eprintln!("Discarding pre-release workflow: {}", workflow.0);
                }
                WorkflowPins::Both { stable, prerelease } => {
                    eprintln!("Discarding stable workflow: {}", stable.0);
                    eprintln!("Discarding pre-release workflow: {}", prerelease.0);
                }
            }
        }
    }
    eprintln!("The ratcheted signature requirement and all other lock state will be retained.");
    flush_preview()?;

    if !confirm_recovery(provider, "provider source", force)? {
        return Ok(());
    }
    recovery
        .commit()
        .map_err(|error| AppError::Config(error.to_string()))?;
    save_lock(&mut lock)?;
    println!(
        "Signer pins cleared. Run `{}` to verify a signed artifact and acquire new pins.",
        ProjectCommand::new("init", base_dir)
    );
    Ok(())
}

fn display_sequence(value: Option<u64>) -> String {
    value
        .map(|sequence| sequence.to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn run_rebootstrap(provider: &str, force: bool, base_dir: &Path) -> Result<(), AppError> {
    let install_lock = acquire_install_lock(base_dir)?;
    let mut lock = load_lock(&install_lock)?;
    let migration = lock.take_migration();
    report_lock_migration(migration.as_ref());
    let recovery = lock
        .prepare_registry_rebootstrap(provider)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let freshness = recovery.freshness();

    eprintln!("Registry provider: {provider}");
    eprintln!(
        "Discarding sequence observation: {}",
        display_sequence(freshness.sequence)
    );
    eprintln!(
        "Discarding sequence anchor: {} (persisted reference point)",
        display_sequence(freshness.sequence_anchor)
    );
    eprintln!("All other provider lock state will be retained.");
    flush_preview()?;

    if !confirm_recovery(provider, "provider source", force)? {
        return Ok(());
    }
    // A pinned target persists a typed reset-pending anchor state so failed
    // resolutions cannot consume the operator-authorized reset. A pre-pin
    // observation has no anchor; clearing it leaves the source at first
    // contact. The next successful resolution pins either case.
    recovery
        .commit()
        .map_err(|error| AppError::Config(error.to_string()))?;
    save_lock(&mut lock)?;
    println!(
        "Registry freshness reset. Run `{}` to establish a new reference point (or record that the registry serves no sequence).",
        ProjectCommand::new("init", base_dir)
    );
    Ok(())
}
