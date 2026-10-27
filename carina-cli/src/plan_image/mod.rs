//! Dependency-graph images for `carina plan`.

mod inline;
mod layout;
mod model;
mod png;
mod svg;

use std::path::{Path, PathBuf};

pub use inline::{
    InlineImageProtocol, InlineImageRequest, RenderedInlineImage, TerminalSize,
    detect_inline_image_protocol, terminal_size, write_inline_image,
};
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
    #[error(
        "cannot use --image-inline because stdout is not a terminal; supported terminals are Kitty, Ghostty, iTerm2, and WezTerm; use --image <FILE> instead"
    )]
    InlineImageStdoutNotTerminal,
    #[error(
        "--image-inline is not supported inside tmux because graphics passthrough is out of scope; use --image <FILE> instead"
    )]
    InlineImageTmuxUnsupported,
    #[error(
        "could not detect a terminal supported by --image-inline; supported terminals are Kitty, Ghostty, iTerm2, and WezTerm; use --image <FILE> instead"
    )]
    InlineImageUnsupportedTerminal,
    #[error("failed to write inline plan image to stdout: {error}")]
    InlineImageWrite { error: std::io::Error },
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

/// Render optional file and terminal image outputs from one SVG/PNG pipeline.
///
/// If both outputs need PNG, rasterization and pixel-budget validation happen
/// only once. Both results are complete in memory before either can be written.
pub fn render_plan_outputs(
    target: Option<&PlanImageTarget>,
    inline: Option<InlineImageRequest>,
    input: PlanImageInput<'_>,
) -> Result<(Option<RenderedPlanImage>, Option<RenderedInlineImage>), PlanImageError> {
    let model = model::build_model(input);
    let layout = layout::layout_graph(&model);
    let svg = svg::render_svg(&model, &layout);
    let needs_png =
        target.is_some_and(|target| target.format == ImageFormat::Png) || inline.is_some();
    let mut png = needs_png.then(|| png::render_png(&svg)).transpose()?;

    let inline_image = inline.map(|request| {
        let png_width = u32::try_from(u64::from(svg.width) * 2)
            .expect("successful PNG rendering validated its scaled width");
        inline::encode_inline_image(
            request,
            png.as_deref()
                .expect("inline rendering requested PNG bytes"),
            png_width,
        )
    });

    let file_image = target.map(|target| {
        let bytes = match target.format {
            ImageFormat::Svg => svg.text.into_bytes(),
            ImageFormat::Png => png.take().expect("PNG file rendering requested PNG bytes"),
        };
        RenderedPlanImage {
            target: target.clone(),
            bytes,
        }
    });

    Ok((file_image, inline_image))
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
    let (image, _) = render_plan_outputs(Some(target), None, input)?;
    Ok(image.expect("a file target always produces a rendered file image"))
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
