//! Dependency-graph images for `carina plan`.

mod layout;
mod model;
mod png;
mod svg;

use std::path::{Path, PathBuf};

pub use model::{PlanImageInput, StateMetadata};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Svg,
    Png,
}

impl ImageFormat {
    fn from_path(path: &Path) -> Result<Self, PlanImageError> {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("svg") => Ok(Self::Svg),
            Some("png") => Ok(Self::Png),
            _ => Err(PlanImageError::UnsupportedExtension {
                path: path.to_path_buf(),
            }),
        }
    }
}

/// A validated plan-image destination whose format cannot disagree with its
/// extension. The private fields make extension validation a construction-time
/// invariant shared by the CLI and fixture harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanImageTarget {
    path: PathBuf,
    format: ImageFormat,
}

impl PlanImageTarget {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TryFrom<PathBuf> for PlanImageTarget {
    type Error = PlanImageError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        let format = ImageFormat::from_path(&path)?;
        Ok(Self { path, format })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PlanImageError {
    #[error("plan image path must end in .svg or .png: {path}", path = .path.display())]
    UnsupportedExtension { path: PathBuf },
    #[error("failed to write plan image {path}: {error}")]
    Write {
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("failed to parse generated plan SVG: {error}")]
    SvgParse { error: resvg::usvg::Error },
    #[error("plan image dimensions are too large for PNG rendering")]
    PngDimensions,
    #[error(
        "plan image PNG would be {width}x{height} ({pixels} pixels), above the {max_pixels}-pixel limit; write .svg instead"
    )]
    PngPixelBudget {
        width: u64,
        height: u64,
        pixels: u64,
        max_pixels: u64,
    },
    #[error("failed to encode plan image PNG: {error}")]
    PngEncoding {
        error: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Clap-compatible construction for the `--image` target.
pub fn parse_output_target(value: &str) -> Result<PlanImageTarget, PlanImageError> {
    PlanImageTarget::try_from(PathBuf::from(value))
}

/// Fully rendered image bytes bound to their validated destination.
///
/// Keeping the target inside this value prevents a caller from rendering one
/// format and later writing those bytes to a differently typed target.
#[derive(Debug)]
pub struct RenderedPlanImage {
    target: PlanImageTarget,
    bytes: Vec<u8>,
}

impl RenderedPlanImage {
    pub fn path(&self) -> &Path {
        self.target.path()
    }
}

/// Render a plan image completely in memory using the target's validated
/// format. PNG size validation happens here, before any output file is
/// touched.
pub fn render_plan_image(
    target: &PlanImageTarget,
    input: PlanImageInput<'_>,
) -> Result<RenderedPlanImage, PlanImageError> {
    let model = model::build_model(input);
    let layout = layout::layout_graph(&model);
    let svg = svg::render_svg(&model, &layout);
    let bytes = match target.format {
        ImageFormat::Svg => svg.text.into_bytes(),
        ImageFormat::Png => png::render_png(&svg)?,
    };
    Ok(RenderedPlanImage {
        target: target.clone(),
        bytes,
    })
}

/// Write bytes that have already been rendered and validated.
pub fn write_plan_image(image: RenderedPlanImage) -> Result<(), PlanImageError> {
    std::fs::write(&image.target.path, image.bytes).map_err(|error| PlanImageError::Write {
        path: image.target.path,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_extension_is_case_insensitive() {
        let svg = PlanImageTarget::try_from(PathBuf::from("plan.SVG")).unwrap();
        let png = PlanImageTarget::try_from(PathBuf::from("plan.PnG")).unwrap();

        assert_eq!(svg.path(), Path::new("plan.SVG"));
        assert_eq!(svg.format, ImageFormat::Svg);
        assert_eq!(png.path(), Path::new("plan.PnG"));
        assert_eq!(png.format, ImageFormat::Png);
    }

    #[test]
    fn unsupported_output_extension_is_actionable() {
        let error = PlanImageTarget::try_from(PathBuf::from("plan.jpg")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "plan image path must end in .svg or .png: plan.jpg"
        );
    }
}
