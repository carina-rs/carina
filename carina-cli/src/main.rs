mod commands;
mod display;
mod error;
mod signal;
mod wiring;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{CompleteEnv, Shell, generate};
use colored::Colorize;

use base64::Engine;
use commands::apply::{run_apply, run_apply_from_plan};
use commands::destroy::run_destroy;
use commands::docs;
use commands::fmt::run_fmt;
use commands::lint::run_lint;
use commands::module::{ModuleCommands, run_module_command};
use commands::plan::run_plan;
use commands::skills;
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
#[command(version)]
#[command(about = "A functional infrastructure management tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate the configuration file
    Validate {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Output results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show execution plan without applying changes
    Plan {
        /// Path to directory containing .crn files
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

        /// Output plan as JSON
        #[arg(long)]
        json: bool,

        /// Accept a changed backend configuration and overwrite the local backend lock
        #[arg(long)]
        reconfigure: bool,
    },
    /// Apply changes to reach the desired state
    Apply {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,

        /// Enable/disable state locking (default: true)
        #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
        lock: bool,

        /// Accept a changed backend configuration and overwrite the local backend lock
        #[arg(long)]
        reconfigure: bool,
    },
    /// Destroy all resources defined in the configuration file
    Destroy {
        /// Path to directory containing .crn files
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

        /// Force destroy even if resources have prevent_destroy set
        #[arg(long)]
        force: bool,

        /// Accept a changed backend configuration and overwrite the local backend lock
        #[arg(long)]
        reconfigure: bool,
    },
    /// Show export values from the state
    Export {
        /// Name of a specific export to display
        #[arg()]
        name: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Output raw value without key or quotes (requires a specific export name)
        #[arg(long)]
        raw: bool,
    },
    /// Format .crn files
    Fmt {
        /// Path to directory containing .crn files
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

        /// Path to directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// State management commands
    State {
        #[command(subcommand)]
        command: StateCommands,
    },
    /// Download and install provider binaries
    Init {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Re-resolve all provider versions from constraints, ignoring lock file
        #[arg(long)]
        upgrade: bool,
    },
    /// Lint .crn files for style issues
    Lint {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Manage Agent Skills (install/update/uninstall SKILL.md)
    Skills {
        #[command(subcommand)]
        command: SkillsCommands,
    },
    /// Display embedded documentation
    Docs {
        /// List all available documents
        #[arg(long)]
        list: bool,

        /// Search documents for a keyword
        #[arg(long)]
        search: Option<String>,

        /// Show a specific document by name
        #[arg()]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum SkillsCommands {
    /// List embedded skills
    List,
    /// Install skills to ~/.agents/skills/carina/
    Install,
    /// Update installed skills to the embedded version
    Update,
    /// Reinstall skills (force overwrite)
    Reinstall,
    /// Remove installed skills
    Uninstall,
    /// Show install status and version comparison
    Status,
}

/// Create the parser configuration with AWS KMS decryptor.
///
/// Uses the tokio runtime to call KMS synchronously from within the parse-time
/// builtin evaluation. AWS credentials are loaded from the default chain
/// (environment variables, profiles, instance metadata, etc.).
fn create_provider_context() -> carina_core::parser::ProviderContext {
    static KMS_CLIENT: tokio::sync::OnceCell<aws_sdk_kms::Client> =
        tokio::sync::OnceCell::const_new();

    carina_core::parser::ProviderContext {
        decryptor: Some(Box::new(|ciphertext, key| {
            let ciphertext = ciphertext.to_string();
            let key = key.map(|k| k.to_string());

            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let client = KMS_CLIENT
                        .get_or_init(|| async {
                            let config =
                                aws_config::load_defaults(aws_config::BehaviorVersion::latest())
                                    .await;
                            aws_sdk_kms::Client::new(&config)
                        })
                        .await;

                    let blob = base64::engine::general_purpose::STANDARD
                        .decode(&ciphertext)
                        .map_err(|e| format!("decrypt(): invalid base64 ciphertext: {e}"))?;

                    let mut req = client
                        .decrypt()
                        .ciphertext_blob(aws_sdk_kms::primitives::Blob::new(blob));
                    if let Some(k) = key {
                        req = req.key_id(k);
                    }

                    let resp = req
                        .send()
                        .await
                        .map_err(|e| format!("decrypt(): KMS decrypt failed: {e}"))?;

                    let plaintext = resp.plaintext().ok_or_else(|| {
                        "decrypt(): KMS response contained no plaintext".to_string()
                    })?;

                    String::from_utf8(plaintext.as_ref().to_vec())
                        .map_err(|e| format!("decrypt(): decrypted value is not valid UTF-8: {e}"))
                })
            })
        })),
        validators: std::collections::HashMap::new(),
        custom_type_validator: None,
    }
}

#[tokio::main]
async fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Create parser configuration with AWS KMS decryptor.
    // This must happen before any .crn parsing so that decrypt() calls can be evaluated.
    let provider_context = create_provider_context();

    let cli = Cli::parse();

    // Handle Plan separately since it returns Result<bool, String>
    if let Commands::Plan {
        path,
        out,
        detailed_exitcode,
        detail,
        tui,
        refresh,
        json,
        reconfigure,
    } = cli.command
    {
        match run_plan(
            &path,
            out.as_ref(),
            detail,
            tui,
            refresh,
            json,
            reconfigure,
            &provider_context,
        )
        .await
        {
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
        Commands::Validate { path, json } => run_validate(&path, json, &provider_context),
        Commands::Plan { .. } => unreachable!(),
        Commands::Apply {
            path,
            auto_approve,
            lock,
            reconfigure,
        } => {
            if path.extension().is_some_and(|ext| ext == "json") {
                run_apply_from_plan(&path, auto_approve, lock).await
            } else {
                run_apply(&path, auto_approve, lock, reconfigure, &provider_context).await
            }
        }
        Commands::Destroy {
            path,
            auto_approve,
            lock,
            refresh,
            force,
            reconfigure,
        } => {
            run_destroy(
                &path,
                auto_approve,
                lock,
                refresh,
                force,
                reconfigure,
                &provider_context,
            )
            .await
        }
        Commands::Export { name, json, raw } => {
            let format = if raw {
                commands::export::OutputFormat::Raw
            } else if json {
                commands::export::OutputFormat::Json
            } else {
                commands::export::OutputFormat::Human
            };
            let path = PathBuf::from(".");
            commands::export::run_export(&path, name, format, &provider_context).await
        }
        Commands::Fmt {
            path,
            check,
            diff,
            recursive,
        } => run_fmt(&path, check, diff, recursive),
        Commands::Module { command } => run_module_command(command, &provider_context),
        Commands::ForceUnlock { lock_id, path } => {
            run_force_unlock(&lock_id, &path, &provider_context).await
        }
        Commands::State { command } => run_state_command(command, &provider_context).await,
        Commands::Init { path, upgrade } => {
            if let Err(e) = commands::init::run_init(&path, upgrade) {
                eprintln!("{}", format!("Error: {e}").red());
                std::process::exit(1);
            }
            Ok(())
        }
        Commands::Lint { path } => run_lint(&path, &provider_context),
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "carina", &mut std::io::stdout());
            Ok(())
        }
        Commands::Skills { command } => {
            let output = match command {
                SkillsCommands::List => Ok(skills::run_skills_list()),
                SkillsCommands::Install => skills::run_skills_install(),
                SkillsCommands::Update => skills::run_skills_update(),
                SkillsCommands::Reinstall => skills::run_skills_reinstall(),
                SkillsCommands::Uninstall => skills::run_skills_uninstall(),
                SkillsCommands::Status => skills::run_skills_status(),
            };
            match output {
                Ok(text) => {
                    println!("{text}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        Commands::Docs { list, search, name } => {
            let output: Result<String, error::AppError> = if list {
                Ok(docs::run_docs_list())
            } else if let Some(query) = search {
                Ok(docs::run_docs_search(&query))
            } else if let Some(doc_name) = name {
                docs::run_docs_show(&doc_name)
            } else {
                Ok(docs::run_docs_default())
            };
            match output {
                Ok(text) => {
                    println!("{text}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };

    if let Err(e) = result {
        match e {
            error::AppError::Interrupted => {
                // Exit code 130 = 128 + 2 (SIGINT), the Unix convention
                std::process::exit(130);
            }
            _ => {
                eprintln!("{} {}", "Error:".red().bold(), e);
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod cli_version_tests;
#[cfg(test)]
mod module_info_snapshot_tests;
#[cfg(test)]
mod module_list_tests;
#[cfg(test)]
mod plan_snapshot_tests;
#[cfg(test)]
mod tests;
