use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{CompleteEnv, Shell, generate};
use colored::Colorize;

use carina_cli::commands;
use carina_cli::commands::apply::{run_apply, run_apply_from_plan};
use carina_cli::commands::destroy::run_destroy;
use carina_cli::commands::docs;
use carina_cli::commands::fmt::run_fmt;
use carina_cli::commands::lint::run_lint;
use carina_cli::commands::module::{ModuleCommands, run_module_command};
use carina_cli::commands::plan::run_plan;
use carina_cli::commands::skills;
use carina_cli::commands::state::{StateCommands, run_force_unlock, run_state_command};
use carina_cli::commands::validate::run_validate;
use carina_cli::error;
use carina_cli::{DEFAULT_PARALLELISM, DetailLevel};

/// Version string assembled at build time by `build.rs`. Formatted as
/// `<pkg> (<git-hash>[-dirty] <build-date>)`, or just `<pkg>` when the
/// binary is built from a non-git source (e.g. `cargo install` from
/// crates.io, where `build.rs` has no git context).
const VERSION: &str = env!("CARINA_VERSION_STRING");

#[derive(Parser)]
#[command(name = "carina")]
#[command(version = VERSION)]
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

        /// Pre-check apply role's IAM permissions against the actions providers declare. Emits warnings; does not fail the plan.
        #[arg(long)]
        check_iam: bool,

        /// With --check-iam, fail (exit 1) instead of warning when permissions are missing. Requires --check-iam.
        #[arg(long, requires = "check_iam")]
        strict_iam: bool,
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

        /// Maximum concurrent provider operations
        #[arg(long, default_value_t = DEFAULT_PARALLELISM)]
        parallelism: NonZeroUsize,
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

        /// Maximum concurrent provider operations
        #[arg(long, default_value_t = DEFAULT_PARALLELISM)]
        parallelism: NonZeroUsize,
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
    /// Download and install provider binaries; migrate state on backend change
    Init {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Re-resolve all provider versions from constraints, ignoring lock file
        #[arg(long)]
        upgrade: bool,
        /// Require the lock file to match providers.crn exactly; error if any
        /// provider is missing from the lock. Intended for CI (like cargo --locked).
        #[arg(long, conflicts_with = "upgrade")]
        locked: bool,
        /// Migrate the state file from the backend recorded in
        /// carina-backend.lock to the currently configured backend when
        /// they differ. Without this flag, a backend change is a hard error.
        #[arg(long)]
        migrate_state: bool,
        /// When migrating, overwrite a target backend that already
        /// contains a different state. Has no effect without
        /// --migrate-state.
        #[arg(long, requires = "migrate_state")]
        force: bool,
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
/// builtin evaluation. AWS credentials are loaded lazily on the first
/// `decrypt()` call from the default chain (environment variables, profiles,
/// instance metadata, etc.) and cached in a process-wide `OnceCell` so the
/// SDK is never initialised for commands that do not use `decrypt()`.
///
/// The per-call body (base64 → `KMS:Decrypt` → UTF-8) lives in
/// [`carina_cli::kms::decrypt_one`] so an integration test can drive it
/// with a mock client (#3227).
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
                    carina_cli::kms::decrypt_one(client, &ciphertext, key.as_deref()).await
                })
            })
        })),
        validators: std::collections::HashMap::new(),
        custom_type_validator: None,
        resource_types: Default::default(),
        // Schemas not yet loaded — early-parse runs before
        // `enrich_provider_context` populates the validator set, so the
        // carina#3239 strict check is deferred to that later context.
        customs_loaded: false,
    }
}

#[tokio::main]
async fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Restore the terminal cursor on the exit paths the command-wide
    // `CursorGuard`'s `Drop` cannot reach — SIGINT/SIGTERM and panic
    // (#3153, #3158).
    carina_cli::cursor::install_restore_handlers();

    // Create parser configuration with AWS KMS decryptor.
    // This must happen before any .crn parsing so that decrypt() calls can be evaluated.
    let provider_context = create_provider_context();

    let cli = Cli::parse();

    // Command-wide cursor hide (#3158 — rationale in `cursor.rs`). Placed
    // after `Cli::parse()` because that exits the process itself on
    // --help/--version/parse error, so those paths must not hide; and
    // before dispatch so all user-visible output runs cursor-hidden.
    let _cursor_guard = carina_cli::cursor::CursorGuard::stdout();

    // Handle Plan separately since it returns Result<bool, String>
    if let Commands::Plan {
        path,
        out,
        detailed_exitcode,
        detail,
        tui,
        refresh,
        json,
        check_iam,
        strict_iam,
    } = cli.command
    {
        match run_plan(
            &path,
            out.as_deref(),
            detail,
            tui,
            refresh,
            json,
            check_iam,
            strict_iam,
            &provider_context,
        )
        .await
        {
            Ok(has_changes) => {
                if detailed_exitcode && has_changes {
                    // process::exit skips Drop — restore the cursor first
                    // (#3158); claim-once with the guard/net.
                    carina_cli::cursor::restore_cursor();
                    std::process::exit(2);
                }
            }
            Err(e) => {
                handle_app_error(e);
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
            parallelism,
        } => {
            // TODO(T7/T8): replace this fresh token with one fed by the signal listener.
            // Until then, real SIGINT/SIGTERM still drops the future via signal::run_with_ctrl_c.
            let cancel_token = tokio_util::sync::CancellationToken::new();
            if path.extension().is_some_and(|ext| ext == "json") {
                run_apply_from_plan(
                    &path,
                    auto_approve,
                    lock,
                    parallelism,
                    &provider_context,
                    cancel_token,
                )
                .await
            } else {
                run_apply(
                    &path,
                    auto_approve,
                    lock,
                    parallelism,
                    &provider_context,
                    cancel_token,
                )
                .await
            }
        }
        Commands::Destroy {
            path,
            auto_approve,
            lock,
            refresh,
            force,
            parallelism,
        } => {
            // TODO(T7/T8): replace this fresh token with one fed by the signal listener.
            // Until then, real SIGINT/SIGTERM still drops the future via signal::run_with_ctrl_c.
            let cancel_token = tokio_util::sync::CancellationToken::new();
            run_destroy(
                &path,
                auto_approve,
                lock,
                refresh,
                force,
                parallelism,
                &provider_context,
                cancel_token,
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
        Commands::Init {
            path,
            upgrade,
            locked,
            migrate_state,
            force,
        } => {
            if let Err(e) =
                commands::init::run_init(&path, upgrade, locked, migrate_state, force).await
            {
                // process::exit skips Drop — restore the cursor first
                // (#3158); claim-once with the guard/net.
                carina_cli::cursor::restore_cursor();
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
        handle_app_error(e);
    }
}

/// Outcome of rendering an `AppError`: the text to write to stderr
/// and the exit code to terminate with.
struct AppErrorRendering {
    stderr: String,
    exit_code: i32,
}

/// Pure rendering of `AppError`. Split out from `handle_app_error` so
/// the formatting can be tested without invoking `process::exit`.
fn render_app_error(e: &error::AppError) -> AppErrorRendering {
    match e {
        error::AppError::Interrupted => AppErrorRendering {
            stderr: String::new(),
            // Exit code 130 = 128 + 2 (SIGINT), the Unix convention
            exit_code: 130,
        },
        error::AppError::Provider(pe) => {
            let detail = pe.detail();
            let body =
                error::format_account_guard_error(&detail.message, detail.provider_name.as_deref())
                    .unwrap_or_else(|| e.to_string());
            AppErrorRendering {
                stderr: format_error_lines(&body),
                exit_code: 1,
            }
        }
        _ => AppErrorRendering {
            stderr: format_error_lines(&e.to_string()),
            exit_code: 1,
        },
    }
}

/// Render a top-level `AppError` and exit. Provider init failures get
/// the structured account-guard rendering when the message shape
/// matches; everything else falls through to the generic `Error: ...`
/// formatter (#2407).
fn handle_app_error(e: error::AppError) -> ! {
    // `process::exit` runs no destructors, so the command-wide CursorGuard
    // would never restore the cursor on the error path — restore it here
    // first (#3158). Claim-once, so this is harmless if a guard also ran.
    carina_cli::cursor::restore_cursor();
    let rendering = render_app_error(&e);
    if !rendering.stderr.is_empty() {
        eprint!("{}", rendering.stderr);
    }
    std::process::exit(rendering.exit_code);
}

fn format_error_lines(msg: &str) -> String {
    let prefix = "Error:".red().bold().to_string();
    msg.lines()
        .map(|line| format!("{} {}\n", prefix, line))
        .collect()
}

#[cfg(test)]
mod cli_version_tests;

#[cfg(test)]
mod error_format_tests {
    use super::*;

    #[test]
    fn single_line_error_has_prefix() {
        colored::control::set_override(false);
        let result = format_error_lines("something went wrong");
        assert_eq!(result, "Error: something went wrong\n");
    }

    #[test]
    fn multi_line_error_each_line_has_prefix() {
        colored::control::set_override(false);
        let result = format_error_lines("first error\nsecond error");
        assert_eq!(result, "Error: first error\nError: second error\n");
    }

    // -- #2407 acceptance: AppError -> stderr rendering --

    #[test]
    fn provider_account_guard_renders_structured_block() {
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "Provider initialization failed: AWS account ID '019115212452' \
             is not in the provider's allowed_account_ids [\"151116838382\"]. \
             Refusing to operate against this account. \
             Check the AWS credentials in your environment.",
        );
        let app_err: error::AppError = pe.into();
        let r = render_app_error(&app_err);
        assert_eq!(r.exit_code, 1);
        assert!(
            r.stderr.contains("AWS account mismatch"),
            "header missing: {}",
            r.stderr
        );
        assert!(
            r.stderr
                .contains("Expected:    151116838382 (allowed_account_ids)"),
            "expected line missing: {}",
            r.stderr
        );
        assert!(
            r.stderr.contains("Actual:      019115212452"),
            "actual line missing: {}",
            r.stderr
        );
        // Acceptance criteria from #2407.
        assert!(
            !r.stderr.contains("panicked"),
            "must not surface panic framing: {}",
            r.stderr
        );
        assert!(
            !r.stderr.contains("RUST_BACKTRACE"),
            "must not surface backtrace hint: {}",
            r.stderr
        );
        assert!(
            !r.stderr.contains("WASM provider instance"),
            "must not leak WASM hosting detail: {}",
            r.stderr
        );
        assert!(
            !r.stderr.contains("wasm_factory.rs"),
            "must not surface source-file path: {}",
            r.stderr
        );
    }

    #[test]
    fn provider_non_account_guard_falls_back_to_plain_error() {
        // Generic provider init failures (invalid region, missing
        // credentials chain, etc.) must still be surfaced — just not
        // through the structured account-guard renderer.
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "Provider initialization failed: failed to load AWS credentials \
             from the environment",
        );
        let app_err: error::AppError = pe.into();
        let r = render_app_error(&app_err);
        assert_eq!(r.exit_code, 1);
        assert!(
            r.stderr.starts_with("Error: "),
            "missing Error: prefix, got: {}",
            r.stderr
        );
        assert!(
            r.stderr.contains("failed to load AWS credentials"),
            "inner message missing: {}",
            r.stderr
        );
        // Must NOT have promoted itself to the structured shape.
        assert!(
            !r.stderr.contains("AWS account mismatch"),
            "non-account-guard must not be coerced into structured shape: {}",
            r.stderr
        );
        // Acceptance: still no panic / backtrace / WASM leak.
        assert!(!r.stderr.contains("panicked"));
        assert!(!r.stderr.contains("RUST_BACKTRACE"));
        assert!(!r.stderr.contains("WASM provider instance"));
    }

    #[test]
    fn provider_account_guard_uses_attached_provider_name() {
        // When the wiring layer has attached a provider name via
        // ProviderError::for_provider, the structured renderer uses
        // it instead of the "aws" default. Catches awscc-vs-aws
        // mislabeling.
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "Provider initialization failed: AWS account ID '019115212452' \
             is not in the provider's allowed_account_ids [\"151116838382\"]. \
             Refusing to operate against this account.",
        )
        .for_provider("awscc");
        let app_err: error::AppError = pe.into();
        let r = render_app_error(&app_err);
        assert!(
            r.stderr.contains("Provider:    awscc"),
            "provider label not picked up from ProviderError: {}",
            r.stderr
        );
    }

    #[test]
    fn interrupted_error_renders_quietly_with_sigint_exit_code() {
        let r = render_app_error(&error::AppError::Interrupted);
        assert_eq!(r.exit_code, 130);
        assert!(r.stderr.is_empty(), "interrupted stderr: {}", r.stderr);
    }

    #[test]
    fn plan_check_iam_flag_parses() {
        assert!(Cli::try_parse_from(["carina", "plan", "--check-iam"]).is_ok());
    }

    #[test]
    fn plan_iam_flags_have_help_text() {
        let command = Cli::command();
        let plan = command
            .get_subcommands()
            .find(|cmd| cmd.get_name() == "plan")
            .expect("plan subcommand exists");

        for id in ["check_iam", "strict_iam"] {
            let arg = plan
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("{id} argument exists"));
            assert!(arg.get_help().is_some(), "{id} should have help text");
        }
    }

    #[test]
    fn plan_strict_iam_requires_check_iam() {
        assert!(Cli::try_parse_from(["carina", "plan", "--strict-iam"]).is_err());
    }

    #[test]
    fn plan_check_iam_with_strict_iam_parses() {
        assert!(Cli::try_parse_from(["carina", "plan", "--check-iam", "--strict-iam"]).is_ok());
    }
}
