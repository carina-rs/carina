use carina_core::{parser::ProviderConfig, provider_source::ProviderSource};
use carina_provider_resolver::{InstalledProvider, TrustedLocalGrants};
use std::path::{Path, PathBuf};

/// Same accepted layouts and flat-name ambiguity rule as PR 0's identity probe.
/// Authored files remain untouched; only factory-loading projections change.
pub fn project(
    configs: &[(PathBuf, ProviderConfig)],
    root: &Path,
    artifacts: Option<&Path>,
) -> Result<Vec<(PathBuf, ProviderConfig)>, String> {
    let Some(artifacts) = artifacts else {
        return Ok(configs.to_vec());
    };
    let mut projected = Vec::new();
    for (directory, original) in configs {
        let nested = directory
            .strip_prefix(root)
            .ok()
            .map(|relative| artifacts.join(relative));
        let selected = nested
            .as_deref()
            .filter(|path| path.join(".carina/providers").is_dir())
            .unwrap_or(artifacts);
        let flat = if selected.join(".carina/providers").is_dir() {
            selected.join(".carina/providers")
        } else {
            selected.to_path_buf()
        };
        let base = if flat.file_name().is_some_and(|name| name == "providers")
            && flat
                .parent()
                .is_some_and(|path| path.file_name().is_some_and(|name| name == ".carina"))
        {
            flat.parent().unwrap().parent().unwrap().to_path_buf()
        } else {
            selected.to_path_buf()
        };
        let mut config = original.clone();
        if config.is_default() {
            let candidates = flat_candidates(&flat, &config.name)?;
            let path = match candidates.as_slice() {
                [path] => Some(path.clone()),
                [] => match &config.source {
                    Some(ProviderSource::File(source)) => Some(directory.join(source.path())),
                    _ => None,
                },
                _ => return Err(format!("multiple artifacts for provider {}", config.name)),
            };
            if let Some(path) = path {
                config.source = Some(
                    format!("file://{}", path.display())
                        .parse()
                        .map_err(|error| format!("{error}"))?,
                );
                config.version = None;
                config.revision = None;
            }
        }
        projected.push((base, config));
    }
    Ok(projected)
}

pub fn flat_candidates(directory: &Path, provider: &str) -> Result<Vec<PathBuf>, String> {
    if !directory.is_dir() {
        return Err("missing provider artifact directory".into());
    }
    Ok([
        format!("{provider}.wasm"),
        format!("carina_provider_{provider}.wasm"),
        format!("carina-provider-{provider}.wasm"),
    ]
    .into_iter()
    .map(|name| directory.join(name))
    .filter(|path| path.is_file())
    .collect())
}

pub fn installed(
    directory: &Path,
    config: &ProviderConfig,
) -> Result<Option<InstalledProvider>, String> {
    let root =
        carina_core::paths::ResolvedPath::new(directory).map_err(|error| error.to_string())?;
    match &config.source {
        None => Ok(None),
        Some(ProviderSource::File(_)) => {
            carina_provider_resolver::find_file_provider_source(&root, config, &mut |_| {})
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Some(ProviderSource::Remote(_)) => {
            carina_provider_resolver::find_installed_provider(&root, config, &mut |_| {})
                .map(Some)
                .map_err(|error| error.to_string())
        }
    }
}

pub fn grants(configs: &[(PathBuf, ProviderConfig)]) -> Result<TrustedLocalGrants, String> {
    let mut values = Vec::new();
    for (_, config) in configs {
        if let Some(ProviderSource::File(source)) = &config.source {
            let kind = match config.name.as_str() {
                "aws" | "awscc" => "aws",
                "github" => "github",
                _ => continue,
            };
            values.push(format!("{}={kind}", source.path()));
        }
    }
    TrustedLocalGrants::parse(&values, None).map_err(|error| error.to_string())
}
