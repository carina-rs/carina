use std::io::{self, Write};
use std::path::{Path, PathBuf};

use carina_provider_resolver::{LockFile, LockFileMigration};

use crate::error::AppError;

const PROVIDER_LOCK_FILE: &str = "carina-providers.lock";

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
    /// Replace a registry provider's signing-identity pin on its next resolution
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
    /// Reset a registry provider's sequence observation and anchor
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

fn load_lock(base_dir: &Path) -> Result<(PathBuf, LockFile, Option<LockFileMigration>), AppError> {
    let lock_path = base_dir.join(PROVIDER_LOCK_FILE);
    let loaded = LockFile::load(&lock_path)
        .map_err(|error| AppError::Config(error.to_string()))?
        .ok_or_else(|| {
            AppError::Config(format!(
                "Provider lock file {} does not exist; run `carina init` first",
                lock_path.display()
            ))
        })?;
    let (lock, migration) = loaded.into_parts();
    Ok((lock_path, lock, migration))
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

fn save_lock(lock: &LockFile, lock_path: &Path) -> Result<(), AppError> {
    lock.save(lock_path).map_err(|error| {
        AppError::Config(format!(
            "Failed to save provider lock {}: {error}",
            lock_path.display()
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
    // `PreparedRegistryDiscoveryRepin::Display` folds the migration header into
    // its discard preview, so printing this event separately would duplicate it.
    let (lock_path, mut lock, _migration) = load_lock(base_dir)?;
    let recovery = lock
        .prepare_registry_discovery_repin(host)
        .map_err(|error| AppError::Config(error.to_string()))?;
    eprintln!("{recovery}");
    flush_preview()?;

    if !confirm_recovery(host, "registry host", force)? {
        return Ok(());
    }
    recovery.commit();
    save_lock(&lock, &lock_path)?;
    println!(
        "Registry discovery pin cleared. Run `carina init` to verify discovery and acquire the new host pin."
    );
    Ok(())
}

fn run_repin_identity(provider: &str, force: bool, base_dir: &Path) -> Result<(), AppError> {
    let (lock_path, mut lock, migration) = load_lock(base_dir)?;
    report_lock_migration(migration.as_ref());
    let recovery = lock
        .prepare_registry_identity_repin(provider)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let pin = recovery.identity_pin();

    eprintln!("Registry provider: {provider}");
    eprintln!(
        "Discarding certificate identity: {}",
        pin.certificate_identity
    );
    eprintln!("Discarding OIDC issuer: {}", pin.certificate_oidc_issuer);
    eprintln!("The ratcheted signature requirement and all other lock state will be retained.");
    flush_preview()?;

    if !confirm_recovery(provider, "provider source", force)? {
        return Ok(());
    }
    recovery
        .commit()
        .map_err(|error| AppError::Config(error.to_string()))?;
    save_lock(&lock, &lock_path)?;
    println!(
        "Identity pin cleared. Run `carina init` to verify a signed artifact and acquire the new pin."
    );
    Ok(())
}

fn display_sequence(value: Option<u64>) -> String {
    value
        .map(|sequence| sequence.to_string())
        .unwrap_or_else(|| "<none>".to_string())
}

fn run_rebootstrap(provider: &str, force: bool, base_dir: &Path) -> Result<(), AppError> {
    let (lock_path, mut lock, migration) = load_lock(base_dir)?;
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
    recovery
        .commit()
        .map_err(|error| AppError::Config(error.to_string()))?;
    save_lock(&lock, &lock_path)?;
    println!(
        "Registry freshness reset. Run `carina init` to establish a new first-contact anchor."
    );
    Ok(())
}
