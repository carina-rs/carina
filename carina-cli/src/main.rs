mod commands;
mod display;
mod error;
mod wiring;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{CompleteEnv, Shell, generate};
use colored::Colorize;

use commands::apply::{run_apply, run_apply_from_plan};
use commands::destroy::run_destroy;
use commands::fmt::run_fmt;
use commands::lint::run_lint;
use commands::module::{ModuleCommands, run_module_command};
use commands::plan::run_plan;
use commands::state::{StateCommands, run_force_unlock, run_state_command};
use commands::validate::run_validate;

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

#[derive(Parser)]
#[command(name = "carina")]
#[command(about = "A functional infrastructure management tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate the configuration file
    Validate {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show execution plan without applying changes
    Plan {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Save plan to a file for later apply
        #[arg(long = "out")]
        out: Option<PathBuf>,

        /// Return exit code 2 when changes are present
        #[arg(long = "detailed-exitcode")]
        detailed_exitcode: bool,

        /// Detail level for plan output: full (default), explicit, none
        #[arg(long, value_enum, default_value = "full")]
        detail: DetailLevel,

        /// Display plan in interactive TUI mode
        #[arg(long)]
        tui: bool,

        /// Refresh state from provider before planning (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        refresh: bool,
    },
    /// Apply changes to reach the desired state
    Apply {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,

        /// Enable/disable state locking (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        lock: bool,
    },
    /// Destroy all resources defined in the configuration file
    Destroy {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,

        /// Enable/disable state locking (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        lock: bool,

        /// Refresh state from provider before destroying (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        refresh: bool,
    },
    /// Format .crn files
    Fmt {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Check if files are formatted (don't modify)
        #[arg(long, short)]
        check: bool,

        /// Show diff of formatting changes
        #[arg(long)]
        diff: bool,

        /// Recursively format all .crn files in directory
        #[arg(long, short)]
        recursive: bool,
    },
    /// Module management commands
    Module {
        #[command(subcommand)]
        command: ModuleCommands,
    },
    /// Force unlock a stuck state lock
    ForceUnlock {
        /// The lock ID to force unlock
        lock_id: String,

        /// Path to .crn file or directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// State management commands
    State {
        #[command(subcommand)]
        command: StateCommands,
    },
    /// Lint .crn files for style issues
    Lint {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[tokio::main]
async fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = Cli::parse();

    // Handle Plan separately since it returns Result<bool, String>
    if let Commands::Plan {
        path,
        out,
        detailed_exitcode,
        detail,
        tui,
        refresh,
    } = cli.command
    {
        match run_plan(&path, out.as_ref(), detail, tui, refresh).await {
            Ok(has_changes) => {
                if detailed_exitcode && has_changes {
                    std::process::exit(2);
                }
            }
            Err(e) => {
                eprintln!("{} {}", "Error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        return;
    }

    let result = match cli.command {
        Commands::Validate { path } => run_validate(&path),
        Commands::Plan { .. } => unreachable!(),
        Commands::Apply {
            path,
            auto_approve,
            lock,
        } => {
            if path.extension().is_some_and(|ext| ext == "json") {
                run_apply_from_plan(&path, auto_approve, lock).await
            } else {
                run_apply(&path, auto_approve, lock).await
            }
        }
        Commands::Destroy {
            path,
            auto_approve,
            lock,
            refresh,
        } => run_destroy(&path, auto_approve, lock, refresh).await,
        Commands::Fmt {
            path,
            check,
            diff,
            recursive,
        } => run_fmt(&path, check, diff, recursive),
        Commands::Module { command } => run_module_command(command),
        Commands::ForceUnlock { lock_id, path } => run_force_unlock(&lock_id, &path).await,
        Commands::State { command } => run_state_command(command).await,
        Commands::Lint { path } => run_lint(&path),
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "carina", &mut std::io::stdout());
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod module_info_snapshot_tests;
#[cfg(test)]
mod plan_snapshot_tests;
#[cfg(test)]
mod tests;
