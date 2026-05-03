use std::path::Path;

use carina_core::config_loader;
use carina_core::module_resolver;
use carina_core::parser::ProviderContext;

use crate::error::AppError;

#[derive(clap::Subcommand)]
pub enum ModuleCommands {
    /// Show module structure and dependencies
    Info {
        /// Path to module directory
        path: std::path::PathBuf,

        /// Display module info in interactive TUI mode
        #[arg(long)]
        tui: bool,
    },
    /// List imported modules
    List {
        /// Path to directory containing .crn files
        #[arg(default_value = ".")]
        path: std::path::PathBuf,
    },
}

pub fn run_module_command(
    command: ModuleCommands,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    match command {
        ModuleCommands::Info { path, tui } => run_module_info(&path, tui),
        ModuleCommands::List { path } => run_module_list(&path, provider_context),
    }
}

fn run_module_list(path: &Path, provider_context: &ProviderContext) -> Result<(), AppError> {
    let config = config_loader::load_configuration_with_config(
        &path.to_path_buf(),
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let output = format_module_list(&config.parsed.uses);
    print!("{output}");
    Ok(())
}

/// Format the module list output as a string.
pub fn format_module_list(imports: &[carina_core::parser::UseStatement]) -> String {
    if imports.is_empty() {
        return "No modules imported.\n".to_string();
    }

    let mut out = String::from("Modules:\n");
    for import in imports {
        out.push_str(&format!("  {:<12}{}\n", import.alias, import.path));
    }
    out
}

fn run_module_info(path: &Path, tui: bool) -> Result<(), AppError> {
    if path.is_file() {
        return Err(AppError::Config(format!(
            "expected directory, got file: {}",
            path.display()
        )));
    }

    let parsed = module_resolver::load_module_from_directory(path)?;

    let module_name = module_resolver::derive_module_name(path);

    // Build and display the file signature (module or root config)
    let signature =
        carina_core::module::FileSignature::from_parsed_file_with_name(&parsed, &module_name);

    if tui {
        carina_tui::run_module_info(&signature)
            .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
    } else {
        println!("{}", signature.display());
    }

    Ok(())
}
