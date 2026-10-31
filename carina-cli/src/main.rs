use std::io::{ErrorKind, IsTerminal};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{CompleteEnv, Shell, generate};
use colored::Colorize;

use carina_cli::commands;
use carina_cli::commands::apply::{run_apply, run_apply_from_plan};
use carina_cli::commands::destroy::run_destroy;
use carina_cli::commands::docs;
use carina_cli::commands::fmt::{FmtMode, run_fmt};
use carina_cli::commands::lint::run_lint;
use carina_cli::commands::module::{ModuleCommands, run_module_command};
use carina_cli::commands::plan::run_plan;
use carina_cli::commands::providers::{ProvidersCommands, run_providers_command};
use carina_cli::commands::skills;
use carina_cli::commands::state::{StateCommands, run_force_unlock, run_state_command};
use carina_cli::commands::validate::run_validate;
use carina_cli::error;
use carina_cli::plan_image::{ImageTheme, PlanImageTarget, resolve_plan_image_options};
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
    /// Trust a local provider component as aws or github for this process
    #[arg(
        long,
        global = true,
        value_name = "PATH=KIND",
        action = clap::ArgAction::Append
    )]
    trust_local_provider: Vec<String>,

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

        /// Write the dependency graph as SVG or PNG (selected by extension)
        #[arg(long, value_parser = carina_cli::plan_image::parse_output_target)]
        image: Option<PlanImageTarget>,

        /// Display the dependency graph in a supported image-capable terminal
        #[arg(long, conflicts_with_all = ["json", "tui"])]
        image_inline: bool,

        /// Select the plan image palette: light, dark, or output-aware auto
        #[arg(long, value_enum)]
        image_theme: Option<ImageTheme>,

        /// Return exit code 2 when changes are present
        #[arg(long = "detailed-exitcode")]
        detailed_exitcode: bool,

        /// Detail level for plan output: full (default), explicit, none
        #[arg(long, value_enum, default_value = "full")]
        detail: DetailLevel,

        /// Display plan in interactive TUI mode
        #[arg(long)]
        tui: bool,

        /// Refresh managed-resource state before planning; data sources are always read (default: true)
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
        /// Path to a project directory or a plan file written by `carina plan --out`
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

        /// Accept a state file that still uses the pre-Phase-5 name override
        /// format. Without this flag, `carina apply` refuses to run against a
        /// state file that has any name_overrides entry missing the recorded
        /// original DSL value, to ensure the operator has reviewed the drift.
        /// Run `carina plan` first to inspect, then re-run with this flag.
        #[arg(long)]
        accept_legacy_name_overrides: bool,
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

        /// Refresh managed-resource state before destroying (default: true)
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

        /// Read state directly from a URL, bypassing .crn / backend resolution
        #[arg(long)]
        state_url: Option<String>,
    },
    /// Format .crn files
    Fmt {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Check if files are formatted (don't modify)
        #[arg(long, short)]
        check: bool,

        /// Show the formatting diff without writing files
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
    /// Registry provider lock recovery commands
    Providers {
        #[command(subcommand)]
        command: ProvidersCommands,
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
        #[arg(long, conflicts_with_all = ["upgrade", "accept_egress"])]
        locked: bool,
        /// Accept the provider-declared egress hosts for this canonical source
        #[arg(long, value_name = "SOURCE", action = clap::ArgAction::Append)]
        accept_egress: Vec<String>,
        /// Migrate the state file from the backend recorded in
        /// carina-backend.lock to the currently configured backend when
        /// they differ. Without this flag, init only warns about a backend
        /// change and leaves migration pending; apply, destroy, and state
        /// refresh refuse to run until the state is migrated. Read-only
        /// commands (plan, state list/show/lookup, export, and upstream_state
        /// reads) warn and read state from the old backend recorded in
        /// carina-backend.lock.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyInput<'a> {
    Project(&'a Path),
    SavedPlan(&'a Path),
}

#[allow(clippy::result_large_err)]
fn classify_apply_input(path: &Path) -> Result<ApplyInput<'_>, error::AppError> {
    match path.metadata() {
        Ok(metadata) if metadata.is_dir() => Ok(ApplyInput::Project(path)),
        Ok(_) => Ok(ApplyInput::SavedPlan(path)),
        Err(error) if error.kind() == ErrorKind::NotFound => match path.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err(error::AppError::ApplyInputDanglingSymlink {
                    path: path.to_path_buf(),
                    error,
                })
            }
            _ => Err(error::AppError::ApplyInputNotFound {
                path: path.to_path_buf(),
                error,
            }),
        },
        Err(error) => Err(error::AppError::ApplyInputInspection {
            path: path.to_path_buf(),
            error,
        }),
    }
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
        resource_types: Default::default(),
        ..carina_core::parser::ProviderContext::default()
    }
}

#[allow(clippy::result_large_err)]
fn resolve_user_dirs() -> Result<carina_core::UserDirs, error::AppError> {
    // Home discovery is intentionally confined to this binary boundary and
    // invoked only by command arms that use user-owned storage.
    #[expect(clippy::disallowed_methods)]
    carina_core::UserDirs::from_real_home().map_err(Into::into)
}

#[tokio::main]
async fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // Restore the terminal cursor on the panic path the command-wide
    // `CursorGuard`'s `Drop` cannot reach (#3153, #3158).
    carina_cli::cursor::install_panic_restore_hook();

    let cli = Cli::parse();
    let trusted_local_env = std::env::var_os("CARINA_TRUST_LOCAL_PROVIDER");
    carina_cli::wiring::set_trusted_local_grant_inputs(
        cli.trust_local_provider.clone(),
        trusted_local_env,
    )
    .expect("trusted local provider grant inputs are installed once at process startup");
    let plan_images = match &cli.command {
        Commands::Plan {
            image,
            image_inline,
            image_theme,
            ..
        } => match resolve_plan_image_options(
            image.as_ref(),
            *image_inline,
            *image_theme,
            |name| std::env::var(name).ok(),
            std::io::stdout().is_terminal(),
        ) {
            Ok(images) => images,
            Err(error) => handle_app_error(error.into()),
        },
        _ => Default::default(),
    };

    // Create parser configuration with AWS KMS decryptor.
    // This must happen before any .crn parsing so that decrypt() calls can be evaluated.
    let provider_context = create_provider_context();

    // Command-wide cursor hide (#3158 — rationale in `cursor.rs`). Placed
    // after `Cli::parse()` because that exits the process itself on
    // --help/--version/parse error, so those paths must not hide; and
    // before dispatch so all user-visible output runs cursor-hidden.
    let _cursor_guard = carina_cli::cursor::CursorGuard::progress();

    let completion = carina_cli::signal::run_with_shutdown(|shutdown| async move {
        // Handle Plan separately since it returns Result<bool, String>
        if let Commands::Plan {
            path,
            out,
            image: _,
            image_inline: _,
            image_theme: _,
            detailed_exitcode,
            detail,
            tui,
            refresh,
            json,
            check_iam,
            strict_iam,
        } = cli.command
        {
            let user_dirs = match resolve_user_dirs() {
                Ok(user_dirs) => user_dirs,
                Err(error) => return CommandCompletion::Error(error),
            };
            match run_plan(
                &user_dirs,
                &path,
                out.as_deref(),
                plan_images.file(),
                plan_images.inline(),
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
                        return CommandCompletion::Exit(2);
                    }
                    return CommandCompletion::Success;
                }
                Err(e) => {
                    return CommandCompletion::Error(e.with_project_dir(&path));
                }
            }
        }

        let result: Result<(), error::AppError> = async {
            match cli.command {
                Commands::Validate { path, json } => {
                    let user_dirs = resolve_user_dirs()?;
                    run_validate(&user_dirs, &path, json, &provider_context)
                }
                Commands::Plan { .. } => unreachable!(),
                Commands::Apply {
                    path,
                    auto_approve,
                    lock,
                    parallelism,
                    accept_legacy_name_overrides,
                } => {
                    let user_dirs = resolve_user_dirs()?;
                    match classify_apply_input(&path) {
                        Ok(ApplyInput::SavedPlan(plan_path)) => {
                            run_apply_from_plan(
                                &user_dirs,
                                plan_path,
                                auto_approve,
                                lock,
                                parallelism,
                                accept_legacy_name_overrides,
                                &provider_context,
                                shutdown.clone(),
                            )
                            .await
                        }
                        Ok(ApplyInput::Project(project_path)) => run_apply(
                            &user_dirs,
                            project_path,
                            auto_approve,
                            lock,
                            parallelism,
                            accept_legacy_name_overrides,
                            &provider_context,
                            shutdown.clone(),
                        )
                        .await
                        .map_err(|error| error.with_project_dir(project_path)),
                        Err(error) => Err(error),
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
                    let user_dirs = resolve_user_dirs()?;
                    run_destroy(
                        &user_dirs,
                        &path,
                        auto_approve,
                        lock,
                        refresh,
                        force,
                        parallelism,
                        &provider_context,
                        shutdown.clone(),
                    )
                    .await
                    .map_err(|error| error.with_project_dir(&path))
                }
                Commands::Export {
                    name,
                    json,
                    raw,
                    state_url,
                } => {
                    let format = if raw {
                        commands::export::OutputFormat::Raw
                    } else if json {
                        commands::export::OutputFormat::Json
                    } else {
                        commands::export::OutputFormat::Human
                    };
                    let path = PathBuf::from(".");
                    commands::export::run_export(
                        &path,
                        state_url.as_deref(),
                        name,
                        format,
                        &provider_context,
                    )
                    .await
                    .map_err(|error| error.with_project_dir(&path))
                }
                Commands::Fmt {
                    path,
                    check,
                    diff,
                    recursive,
                } => {
                    let user_dirs = resolve_user_dirs()?;
                    run_fmt(
                        &user_dirs,
                        &path,
                        FmtMode::from_flags(check, diff),
                        recursive,
                    )
                }
                Commands::Module { command } => run_module_command(command, &provider_context),
                Commands::ForceUnlock { lock_id, path } => {
                    run_force_unlock(&lock_id, &path, &provider_context).await
                }
                Commands::State { command } => {
                    let user_dirs = resolve_user_dirs()?;
                    let project_dir = command.project_dir().map(Path::to_path_buf);
                    let result =
                        run_state_command(&user_dirs, command, &provider_context, shutdown.clone())
                            .await;
                    match project_dir {
                        Some(project_dir) => {
                            result.map_err(|error| error.with_project_dir(&project_dir))
                        }
                        None => result,
                    }
                }
                Commands::Providers { command } => run_providers_command(command),
                Commands::Init {
                    path,
                    upgrade,
                    locked,
                    accept_egress,
                    migrate_state,
                    force,
                } => {
                    let user_dirs = resolve_user_dirs()?;
                    commands::init::run_init(
                        &user_dirs,
                        &path,
                        upgrade,
                        locked,
                        migrate_state,
                        force,
                        &accept_egress,
                    )
                    .await
                    .map_err(error::AppError::from)
                    .map_err(|error| error.with_project_dir(&path))
                }
                Commands::Lint { path } => {
                    let user_dirs = resolve_user_dirs()?;
                    run_lint(&user_dirs, &path, &provider_context)
                }
                Commands::Completions { shell } => {
                    generate(shell, &mut Cli::command(), "carina", &mut std::io::stdout());
                    Ok(())
                }
                Commands::Skills { command } => {
                    let output = match command {
                        SkillsCommands::List => Ok(skills::run_skills_list()),
                        SkillsCommands::Install => {
                            let user_dirs = resolve_user_dirs()?;
                            skills::run_skills_install(&user_dirs)
                        }
                        SkillsCommands::Update => {
                            let user_dirs = resolve_user_dirs()?;
                            skills::run_skills_update(&user_dirs)
                        }
                        SkillsCommands::Reinstall => {
                            let user_dirs = resolve_user_dirs()?;
                            skills::run_skills_reinstall(&user_dirs)
                        }
                        SkillsCommands::Uninstall => {
                            let user_dirs = resolve_user_dirs()?;
                            skills::run_skills_uninstall(&user_dirs)
                        }
                        SkillsCommands::Status => {
                            let user_dirs = resolve_user_dirs()?;
                            skills::run_skills_status(&user_dirs)
                        }
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
            }
        }
        .await;

        match result {
            Ok(()) => CommandCompletion::Success,
            Err(e) => CommandCompletion::Error(e),
        }
    })
    .await;

    match completion {
        CommandCompletion::Success => {}
        CommandCompletion::Exit(code) => {
            // process::exit skips Drop — restore the cursor first (#3158).
            carina_cli::cursor::restore_cursor();
            std::process::exit(code);
        }
        CommandCompletion::Error(error) => handle_app_error(error),
    }
}

/// A completed command's requested process outcome.
///
/// This value is produced inside the shutdown supervisor and interpreted only
/// after the command future has returned, so ordinary error/status exits cannot
/// bypass command-owned cleanup.
enum CommandCompletion {
    Success,
    Exit(i32),
    Error(error::AppError),
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
            let rendered = pe.to_string();
            let body =
                error::format_account_guard_error(&rendered, detail.provider_name.as_deref())
                    .unwrap_or_else(|| {
                        match (detail.operation.as_deref(), detail.provider_name.as_deref()) {
                            (Some("initialize"), Some(provider_name)) => {
                                format!("Provider: {provider_name}\n{rendered}")
                            }
                            _ => rendered,
                        }
                    });
            AppErrorRendering {
                stderr: format_error_lines(&body),
                exit_code: 1,
            }
        }
        error::AppError::PartialSuccess(message) => AppErrorRendering {
            stderr: format_error_lines(message),
            exit_code: 2,
        },
        error::AppError::StateBucketSeed(seed_error) => {
            let stderr = format!(
                "{}\nimport {{\n  to = {}\n  id = {}\n}}\n",
                format_error_lines(&error::format_error_with_source_chain(seed_error.as_ref())),
                seed_error.resource_address(),
                carina_core::parser::display_string_literal(seed_error.identifier()),
            );
            AppErrorRendering {
                stderr,
                exit_code: 1,
            }
        }
        _ => AppErrorRendering {
            stderr: format_error_lines(&error::format_error_with_source_chain(e)),
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

    fn binding_use_app_error() -> error::AppError {
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

        let parsed = carina_core::parser::parse_file(
            "let source = mock.test.Source { name = \"source\" }\n\
             mock.test.Consumer { name = source }\n",
            std::path::Path::new("test.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("binding-use fixture must parse");
        let mut schemas = SchemaRegistry::new();
        for resource_type in ["test.Source", "test.Consumer"] {
            schemas.insert(
                "mock",
                ResourceSchema::new(resource_type)
                    .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                    .validate()
                    .expect("valid fixture schema"),
            );
        }
        let provider_context = carina_core::parser::ProviderContext::default();
        let module_signatures = carina_core::module_resolver::ResolvedModuleSignatures::new();
        let context = carina_core::validation::ValidationTypeContext::new_providerless(
            &parsed,
            &schemas,
            &provider_context,
            &module_signatures,
        );
        let finding = carina_core::validation::check_bare_binding_uses(&parsed, &context)
            .into_findings()
            .into_iter()
            .next()
            .expect("binding-use finding");
        error::AppError::BindingUse(Box::new(finding))
    }

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

    #[test]
    fn binding_use_error_renders_context_and_typed_source_once() {
        colored::control::set_override(false);
        let rendered = render_app_error(&binding_use_app_error());

        assert!(
            rendered.stderr.contains("Error: test.crn at line 2\n"),
            "located context missing: {}",
            rendered.stderr,
        );
        assert_eq!(
            rendered.stderr.matches("test.crn at line 2").count(),
            1,
            "located context must not be duplicated: {}",
            rendered.stderr,
        );
        assert!(
            rendered
                .stderr
                .contains("Error: 'source' is a resource, not a value"),
            "typed source missing: {}",
            rendered.stderr,
        );
        assert_eq!(
            rendered
                .stderr
                .matches("'source' is a resource, not a value")
                .count(),
            1,
            "typed source must not be duplicated: {}",
            rendered.stderr,
        );
    }

    #[test]
    fn state_bucket_seed_error_renders_source_and_pasteable_import_block() {
        colored::control::set_override(false);
        let error = error::AppError::StateBucketSeed(Box::new(error::StateBucketSeedError::new(
            "state'bucket".to_string(),
            "aws.s3.Bucket 'state_bucket'".to_string(),
            carina_state::BackendError::Configuration("seed write failed".to_string()),
        )));

        let rendered = render_app_error(&error);

        assert_eq!(rendered.exit_code, 1);
        let lead_in =
            "Add the following import block to the configuration and re-run apply to record it:";
        assert!(
            rendered.stderr.contains(lead_in),
            "import lead-in missing: {}",
            rendered.stderr
        );
        assert!(
            rendered
                .stderr
                .contains("Error: Backend configuration error: seed write failed"),
            "backend source missing: {}",
            rendered.stderr
        );
        assert!(
            rendered.stderr.contains(
                "\nimport {\n  to = aws.s3.Bucket 'state_bucket'\n  id = \"state'bucket\"\n}\n"
            ),
            "pasteable import block missing: {}",
            rendered.stderr
        );
        assert!(
            rendered.stderr.find(lead_in) < rendered.stderr.find("\nimport {"),
            "import lead-in must precede the block: {}",
            rendered.stderr
        );
        assert!(
            !rendered.stderr.contains("Error: import {")
                && !rendered.stderr.contains("Error:   to =")
                && !rendered.stderr.contains("Error:   id =")
                && !rendered.stderr.contains("Error: }"),
            "import block must not be prefixed: {}",
            rendered.stderr
        );

        let import_start = rendered
            .stderr
            .find("import {")
            .expect("rendered error should contain an import block");
        let import_block = &rendered.stderr[import_start..];
        let parsed = carina_core::parser::parse_file(
            import_block,
            std::path::Path::new("rendered_import.crn"),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("rendered import block should parse");
        assert_eq!(parsed.state_blocks.len(), 1);
        match &parsed.state_blocks[0] {
            carina_core::parser::StateBlock::Import { to, to_span: _, id } => {
                assert_eq!(to.provider, "aws");
                assert_eq!(to.resource_type, "s3.Bucket");
                assert_eq!(to.name_str(), "state_bucket");
                assert_eq!(
                    id,
                    &carina_core::resource::Value::Concrete(
                        carina_core::resource::ConcreteValue::String("state'bucket".into())
                    )
                );
            }
            other => panic!("expected one import state block, got {other:?}"),
        }
    }

    // -- #2407 acceptance: AppError -> stderr rendering --

    #[test]
    fn provider_account_guard_renders_structured_block() {
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "AWS account ID '019115212452' \
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
    fn decoded_provider_initialization_account_guard_renders_structured_block() {
        colored::control::set_override(false);
        const AWS_MESSAGE: &str = "AWS account ID 019115212452 is not in allowed_account_ids \
            [\"151116838382\"]; refusing to operate against this account";
        const AWSCC_MESSAGE: &str = "AWS account ID '019115212452' is not in the provider's \
            allowed_account_ids [\"151116838382\"]. Refusing to operate against this account. \
            Check the AWS credentials in your environment.";

        for (provider_name, message) in [("aws", AWS_MESSAGE), ("awscc", AWSCC_MESSAGE)] {
            let pe =
                carina_plugin_host::wasm_factory::decode_initialize_string_error_for_test(message)
                    // This is the wiring boundary used by both ordinary and
                    // source-loaded provider construction.
                    .for_provider(provider_name);
            assert_eq!(pe.detail().operation.as_deref(), Some("initialize"));
            let app_err: error::AppError = pe.into();
            let rendered = render_app_error(&app_err);

            assert!(
                rendered.stderr.contains("AWS account mismatch"),
                "decoded account-guard error was dropped for {provider_name}: {}",
                rendered.stderr
            );
            assert!(
                rendered
                    .stderr
                    .contains(&format!("Provider:    {provider_name}")),
                "provider name was lost: {}",
                rendered.stderr
            );
            assert!(rendered.stderr.contains("Actual:      019115212452"));
        }
    }

    #[test]
    fn provider_non_account_guard_falls_back_to_plain_error() {
        // Generic provider init failures (invalid region, missing
        // credentials chain, etc.) must still be surfaced — just not
        // through the structured account-guard renderer.
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "failed to load AWS credentials from the environment",
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
    fn decoded_provider_initialization_fallback_names_provider_and_phase() {
        colored::control::set_override(false);
        let pe = carina_plugin_host::wasm_factory::decode_initialize_string_error_for_test(
            "failed to load AWS credentials from the environment",
        )
        .for_provider("awscc");

        let rendered = render_app_error(&error::AppError::Provider(pe));

        assert_eq!(
            rendered.stderr,
            "Error: Provider: awscc\n\
             Error: failed to load AWS credentials from the environment\n\
             Error:   operation: initialize\n"
        );
    }

    #[test]
    fn non_initialization_provider_error_does_not_add_provider_header() {
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "failed to encode default tag for provider 'aws'",
        )
        .for_provider("aws");

        let rendered = render_app_error(&error::AppError::Provider(pe));

        assert_eq!(
            rendered.stderr,
            "Error: failed to encode default tag for provider 'aws'\n"
        );
    }

    #[test]
    fn provider_account_guard_uses_attached_provider_name() {
        // When the wiring layer has attached a provider name via
        // ProviderError::for_provider, the structured renderer uses
        // it instead of the "aws" default. Catches awscc-vs-aws
        // mislabeling.
        colored::control::set_override(false);
        let pe = carina_core::provider::ProviderError::invalid_input(
            "AWS account ID '019115212452' \
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
    fn trust_local_provider_is_repeatable_and_global() {
        let cli = Cli::try_parse_from([
            "carina",
            "plan",
            "--trust-local-provider",
            "/tmp/aws.wasm=aws",
            "--trust-local-provider",
            "/tmp/github.wasm=github",
        ])
        .unwrap();

        assert_eq!(
            cli.trust_local_provider,
            ["/tmp/aws.wasm=aws", "/tmp/github.wasm=github"]
        );
    }

    #[test]
    fn plan_image_extension_is_validated_during_cli_parsing() {
        let error = match Cli::try_parse_from([
            "carina",
            "plan",
            "--image",
            "review.jpg",
            "directory-that-must-not-be-read",
        ]) {
            Ok(_) => panic!("unsupported image extension must be rejected"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("plan image path must end in .svg or .png: review.jpg")
        );
    }

    #[test]
    fn plan_image_before_project_path_parses() {
        let cli = Cli::try_parse_from(["carina", "plan", "--image", "review.PNG", "infra/foo"])
            .expect("plan --image <file> <dir> should parse");

        match cli.command {
            Commands::Plan { path, image, .. } => {
                assert_eq!(path, PathBuf::from("infra/foo"));
                assert_eq!(image.unwrap().path(), Path::new("review.PNG"));
            }
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn plan_image_theme_values_parse() {
        for (value, expected) in [
            ("light", ImageTheme::Light),
            ("dark", ImageTheme::Dark),
            ("auto", ImageTheme::Auto),
        ] {
            let cli = Cli::try_parse_from(["carina", "plan", "--image-theme", value])
                .expect("supported image theme should parse");

            match cli.command {
                Commands::Plan { image_theme, .. } => {
                    assert_eq!(image_theme, Some(expected), "value: {value}");
                }
                _ => panic!("expected plan command"),
            }
        }
    }

    #[test]
    fn plan_image_theme_is_unset_when_omitted() {
        let cli = Cli::try_parse_from(["carina", "plan"]).unwrap();

        match cli.command {
            Commands::Plan { image_theme, .. } => assert_eq!(image_theme, None),
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn plan_image_theme_rejects_unknown_value() {
        let error = match Cli::try_parse_from(["carina", "plan", "--image-theme", "sepia"]) {
            Ok(_) => panic!("unsupported image theme must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
        let message = error.to_string();
        for value in ["light", "dark", "auto"] {
            assert!(message.contains(value), "{message}");
        }
    }

    #[test]
    fn plan_png_rejects_explicit_auto_theme_during_cli_resolution() {
        let cli = Cli::try_parse_from([
            "carina",
            "plan",
            "--image",
            "review.png",
            "--image-theme",
            "auto",
            "directory-that-must-not-be-read",
        ])
        .unwrap();

        let Commands::Plan {
            image,
            image_inline,
            image_theme,
            ..
        } = cli.command
        else {
            panic!("expected plan command");
        };
        let error =
            resolve_plan_image_options(image.as_ref(), image_inline, image_theme, |_| None, false)
                .unwrap_err();

        assert!(matches!(
            error,
            carina_cli::plan_image::PlanImageError::PngAutoTheme
        ));
    }

    #[test]
    fn plan_image_inline_can_be_combined_with_image() {
        let cli =
            Cli::try_parse_from(["carina", "plan", "--image-inline", "--image", "review.png"])
                .expect("--image-inline and --image should be composable");

        match cli.command {
            Commands::Plan {
                image_inline,
                image,
                ..
            } => {
                assert!(image_inline);
                assert_eq!(image.unwrap().path(), Path::new("review.png"));
            }
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn plan_image_inline_conflicts_with_json() {
        let error = match Cli::try_parse_from(["carina", "plan", "--image-inline", "--json"]) {
            Ok(_) => panic!("inline terminal output must not corrupt JSON stdout"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        let message = error.to_string();
        assert!(message.contains("--image-inline"), "{message}");
        assert!(message.contains("--json"), "{message}");
    }

    #[test]
    fn plan_image_inline_conflicts_with_tui() {
        let error = match Cli::try_parse_from(["carina", "plan", "--image-inline", "--tui"]) {
            Ok(_) => panic!("the TUI must retain exclusive ownership of the terminal"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        let message = error.to_string();
        assert!(message.contains("--image-inline"), "{message}");
        assert!(message.contains("--tui"), "{message}");
    }

    #[test]
    fn plan_out_before_project_path_parses() {
        let cli =
            Cli::try_parse_from(["carina", "plan", "--out", "reviewed-plan.json", "infra/foo"])
                .expect("plan --out <file> <dir> should parse");

        match cli.command {
            Commands::Plan { path, out, .. } => {
                assert_eq!(path, PathBuf::from("infra/foo"));
                assert_eq!(out, Some(PathBuf::from("reviewed-plan.json")));
            }
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn plan_out_path_prefixed_to_protect_leading_dash_parses() {
        let cli = Cli::try_parse_from([
            "carina",
            "plan",
            "--out",
            "./-reviewed-plan.json",
            "infra/foo",
        ])
        .expect("a leading-dash --out value protected with ./ should parse");

        match cli.command {
            Commands::Plan { path, out, .. } => {
                assert_eq!(path, PathBuf::from("infra/foo"));
                assert_eq!(out, Some(PathBuf::from("./-reviewed-plan.json")));
            }
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn force_unlock_accepts_leading_dash_lock_id_after_option_terminator() {
        let cli = Cli::try_parse_from(["carina", "force-unlock", "--", "-lock-id", "infra/foo"])
            .expect("force-unlock should accept a leading-dash lock ID after --");

        match cli.command {
            Commands::ForceUnlock { lock_id, path } => {
                assert_eq!(lock_id, "-lock-id");
                assert_eq!(path, PathBuf::from("infra/foo"));
            }
            _ => panic!("expected force-unlock command"),
        }
    }

    #[test]
    fn state_forget_hint_shape_parses_with_auto_approve() {
        let cli = Cli::try_parse_from([
            "carina",
            "state",
            "forget",
            "awscc.ec2.Vpc",
            "main-vpc",
            "--auto-approve",
        ])
        .expect("the plan hint's state-forget command must be copy-pasteable");

        let Commands::State { command } = cli.command else {
            panic!("expected state command");
        };
        let StateCommands::Forget {
            resource_type,
            identity,
            auto_approve,
            path,
        } = command
        else {
            panic!("expected state forget command");
        };
        assert_eq!(resource_type, "awscc.ec2.Vpc");
        assert_eq!(identity, "main-vpc");
        assert!(auto_approve);
        assert_eq!(path, PathBuf::from("."));
    }

    #[test]
    fn saved_plan_apply_accepts_legacy_override_flag_after_plan_path() {
        let cli = Cli::try_parse_from([
            "carina",
            "apply",
            "reviewed-plan.json",
            "--accept-legacy-name-overrides",
        ])
        .expect("saved-plan apply should accept the legacy override flag");

        match cli.command {
            Commands::Apply {
                path,
                accept_legacy_name_overrides,
                ..
            } => {
                assert_eq!(path, PathBuf::from("reviewed-plan.json"));
                assert!(accept_legacy_name_overrides);
            }
            _ => panic!("expected apply command"),
        }
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
    fn init_migrate_state_help_describes_pending_migration() {
        let mut command = Cli::command();
        command.build();
        let init = command
            .find_subcommand_mut("init")
            .expect("init subcommand exists");
        let help = init.render_long_help().to_string();

        assert!(!help.contains("hard error"), "{help}");
        assert!(help.contains("init only warns"), "{help}");
        assert!(help.contains("migration pending"), "{help}");
        assert!(help.contains("apply, destroy, and state refresh"), "{help}");
        assert!(
            help.contains(
                "Read-only commands (plan, state list/show/lookup, export, and upstream_state \
                 reads) warn and read state from the old backend recorded in carina-backend.lock"
            ),
            "{help}"
        );
    }

    #[test]
    fn plan_strict_iam_requires_check_iam() {
        assert!(Cli::try_parse_from(["carina", "plan", "--strict-iam"]).is_err());
    }

    #[test]
    fn plan_check_iam_with_strict_iam_parses() {
        assert!(Cli::try_parse_from(["carina", "plan", "--check-iam", "--strict-iam"]).is_ok());
    }

    #[test]
    fn provider_recovery_commands_parse_with_optional_path() {
        assert!(
            Cli::try_parse_from([
                "carina",
                "providers",
                "repin-discovery",
                "Registry.Carina-RS.dev",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "carina",
                "providers",
                "repin-discovery",
                "registry.example.test",
                "/tmp/project",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["carina", "providers", "repin-identity", "carina-rs/aws",])
                .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "carina",
                "providers",
                "re-bootstrap",
                "registry.example.test/acme/widget",
                "/tmp/project",
            ])
            .is_ok()
        );
    }
}
