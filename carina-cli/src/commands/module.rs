use std::path::Path;

use carina_core::module_resolver;

use crate::error::AppError;

#[derive(clap::Subcommand)]
pub enum ModuleCommands {
    /// Show module structure and dependencies
    Info {
        /// Path to module .crn file
        file: std::path::PathBuf,
    },
}

pub fn run_module_command(command: ModuleCommands) -> Result<(), AppError> {
    match command {
        ModuleCommands::Info { file } => run_module_info(&file),
    }
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
