use std::path::Path;

use carina_core::config_loader;
use carina_core::module_resolver;

use crate::error::AppError;

#[derive(clap::Subcommand)]
pub enum ModuleCommands {
    /// Show module structure and dependencies
    Info {
        /// Path to module .crn file
        file: std::path::PathBuf,
    },
    /// List imported modules
    List {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: std::path::PathBuf,
    },
}

pub fn run_module_command(command: ModuleCommands) -> Result<(), AppError> {
    match command {
        ModuleCommands::Info { file } => run_module_info(&file),
        ModuleCommands::List { path } => run_module_list(&path),
    }
}

fn run_module_list(path: &Path) -> Result<(), AppError> {
    let config = config_loader::load_configuration(&path.to_path_buf())?;
    let output = format_module_list(&config.parsed.imports);
    print!("{output}");
    Ok(())
}

/// Format the module list output as a string.
pub fn format_module_list(imports: &[carina_core::parser::ImportStatement]) -> String {
    if imports.is_empty() {
        return "No modules imported.\n".to_string();
    }

    let mut out = String::from("Modules:\n");
    for import in imports {
        out.push_str(&format!("  {:<12}{}\n", import.alias, import.path));
    }
    out
}

fn run_module_info(path: &Path) -> Result<(), AppError> {
    let parsed = if path.is_dir() {
        // Read all .crn files in the directory and merge them
        module_resolver::load_module_from_directory(path)?
    } else {
        module_resolver::get_parsed_file(path).map_err(|e| format!("Failed to load file: {}", e))?
    };

    // Derive module name from directory structure
    // For directory-based modules like modules/web_tier/, use the directory name
    // For file-based modules like modules/web_tier.crn, use the file stem
    let module_name = module_resolver::derive_module_name(path);

    // Build and display the file signature (module or root config)
    let signature =
        carina_core::module::FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    println!("{}", signature.display());

    Ok(())
}
