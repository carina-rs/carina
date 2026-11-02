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

/// Theme value accepted by `--image-theme` before output-specific defaults
/// and capabilities are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ImageTheme {
    Light,
    Dark,
    Auto,
}

/// A concrete palette suitable for raster output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

/// Theme modes supported by SVG output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvgTheme {
    Fixed(Theme),
    Auto,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedFileTheme {
    Svg(SvgTheme),
    Png(Theme),
}

/// A file destination with an output-compatible, fully resolved theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPlanImageTarget {
    path: PathBuf,
    destination: carina_core::paths::ResolvedPath,
    theme: ResolvedFileTheme,
}

impl ResolvedPlanImageTarget {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Image settings resolved immediately after CLI parsing. No automatic theme
/// or terminal capability remains for the planning path to interpret.
#[derive(Debug, Default)]
pub struct ResolvedPlanImageOptions {
    file: Option<ResolvedPlanImageTarget>,
    inline: Option<InlineImageRequest>,
}

impl ResolvedPlanImageOptions {
    pub fn file(&self) -> Option<&ResolvedPlanImageTarget> {
        self.file.as_ref()
    }

    pub fn inline(&self) -> Option<InlineImageRequest> {
        self.inline
    }

    pub fn is_empty(&self) -> bool {
        self.file.is_none() && self.inline.is_none()
    }
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
    #[error("failed to locate plan image destination {path}: {error}", path = .path.display())]
    Resolve {
        path: PathBuf,
        error: std::io::Error,
    },
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
        "cannot use --image-theme auto with PNG output because PNG colors are fixed at render time; use light or dark, or write an SVG"
    )]
    PngAutoTheme,
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

#[derive(Debug, PartialEq, Eq)]
struct ResolvedImageThemes {
    file: Option<ResolvedFileTheme>,
    inline: Option<Theme>,
}

fn resolve_image_themes<F>(
    target: Option<&PlanImageTarget>,
    inline: bool,
    requested: Option<ImageTheme>,
    resolve_auto_inline: F,
) -> Result<ResolvedImageThemes, PlanImageError>
where
    F: FnOnce() -> Theme,
{
    if target.is_some_and(|target| target.format == ImageFormat::Png)
        && requested == Some(ImageTheme::Auto)
    {
        return Err(PlanImageError::PngAutoTheme);
    }

    let file = target.map(|target| match (target.format, requested) {
        (ImageFormat::Svg, Some(ImageTheme::Auto)) => ResolvedFileTheme::Svg(SvgTheme::Auto),
        (ImageFormat::Svg, Some(ImageTheme::Dark)) => {
            ResolvedFileTheme::Svg(SvgTheme::Fixed(Theme::Dark))
        }
        (ImageFormat::Svg, None | Some(ImageTheme::Light)) => {
            ResolvedFileTheme::Svg(SvgTheme::Fixed(Theme::Light))
        }
        (ImageFormat::Png, Some(ImageTheme::Dark)) => ResolvedFileTheme::Png(Theme::Dark),
        (ImageFormat::Png, None | Some(ImageTheme::Light)) => ResolvedFileTheme::Png(Theme::Light),
        (ImageFormat::Png, Some(ImageTheme::Auto)) => {
            unreachable!("PNG auto theme was rejected before resolution")
        }
    });

    let inline = inline.then(|| match requested {
        Some(ImageTheme::Light) => Theme::Light,
        Some(ImageTheme::Dark) => Theme::Dark,
        None | Some(ImageTheme::Auto) => resolve_auto_inline(),
    });

    Ok(ResolvedImageThemes { file, inline })
}

/// Resolve file and inline image settings before planning begins.
///
/// PNG auto-theme validation happens before terminal inspection. Inline auto
/// mode queries OSC 11 and falls back to light when the terminal cannot report
/// its background.
pub fn resolve_plan_image_options<F>(
    target: Option<&PlanImageTarget>,
    inline: bool,
    requested: Option<ImageTheme>,
    env: F,
    stdout_is_terminal: bool,
) -> Result<ResolvedPlanImageOptions, PlanImageError>
where
    F: FnMut(&str) -> Option<String>,
{
    if target.is_some_and(|target| target.format == ImageFormat::Png)
        && requested == Some(ImageTheme::Auto)
    {
        return Err(PlanImageError::PngAutoTheme);
    }

    let protocol = inline
        .then(|| detect_inline_image_protocol(env, stdout_is_terminal))
        .transpose()?;
    let themes = resolve_image_themes(target, inline, requested, inline::query_terminal_theme)?;
    let file = target
        .zip(themes.file)
        .map(|(target, theme)| {
            let destination =
                carina_core::paths::ResolvedPath::new(&target.path).map_err(|error| {
                    PlanImageError::Resolve {
                        path: target.path.clone(),
                        error,
                    }
                })?;
            Ok(ResolvedPlanImageTarget {
                path: target.path.clone(),
                destination,
                theme,
            })
        })
        .transpose()?;
    let inline = protocol
        .zip(themes.inline)
        .map(|(protocol, theme)| InlineImageRequest {
            protocol,
            terminal: terminal_size(),
            theme,
        });

    Ok(ResolvedPlanImageOptions { file, inline })
}

/// Fully rendered image bytes bound to their validated destination.
///
/// Keeping the target inside this value prevents a caller from rendering one
/// format and later writing those bytes to a differently typed target.
#[derive(Debug)]
pub struct RenderedPlanImage {
    target: ResolvedPlanImageTarget,
    bytes: Vec<u8>,
}

/// Render optional file and terminal image outputs from one SVG/PNG pipeline.
///
/// If both outputs need PNG, rasterization and pixel-budget validation happen
/// only once. Both results are complete in memory before either can be written.
pub fn render_plan_outputs(
    target: Option<&ResolvedPlanImageTarget>,
    inline: Option<InlineImageRequest>,
    input: PlanImageInput<'_>,
) -> Result<(Option<RenderedPlanImage>, Option<RenderedInlineImage>), PlanImageError> {
    let model = model::build_model(input);
    let layout = layout::layout_graph(&model);
    let mut inline_png = inline
        .map(|request| {
            let png = png::render_png(&model, &layout, request.theme)?;
            let png_width = u32::try_from(u64::from(layout.width) * 2)
                .expect("successful PNG rendering validated its scaled width");
            Ok((request, png, png_width))
        })
        .transpose()?;
    let inline_image = inline_png
        .as_ref()
        .map(|(request, png, png_width)| inline::encode_inline_image(*request, png, *png_width));

    let file_image = target
        .map(|target| {
            let bytes = match target.theme {
                ResolvedFileTheme::Svg(theme) => {
                    svg::render_svg(&model, &layout, theme).text.into_bytes()
                }
                ResolvedFileTheme::Png(theme) => {
                    if inline_png
                        .as_ref()
                        .is_some_and(|(request, _, _)| request.theme == theme)
                    {
                        inline_png
                            .take()
                            .expect("matching inline PNG was rendered")
                            .1
                    } else {
                        png::render_png(&model, &layout, theme)?
                    }
                }
            };
            Ok(RenderedPlanImage {
                target: target.clone(),
                bytes,
            })
        })
        .transpose()?;

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
    target: &ResolvedPlanImageTarget,
    input: PlanImageInput<'_>,
) -> Result<RenderedPlanImage, PlanImageError> {
    let (image, _) = render_plan_outputs(Some(target), None, input)?;
    Ok(image.expect("a file target always produces a rendered file image"))
}

/// Write bytes that have already been rendered and validated.
pub fn write_plan_image(image: RenderedPlanImage) -> Result<(), PlanImageError> {
    image
        .target
        .destination
        .atomic_replace(&image.bytes)
        .map_err(|error| PlanImageError::Write {
            path: image.target.path,
            error,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn plan_image_symlink_retarget_keeps_output_in_pinned_directory() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let replacement = temp.path().join("replacement");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        let target = PlanImageTarget::try_from(alias.join("plan.svg")).unwrap();
        let options = resolve_plan_image_options(
            Some(&target),
            false,
            Some(ImageTheme::Light),
            |_| None,
            false,
        )
        .unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&replacement, &alias).unwrap();
        write_plan_image(RenderedPlanImage {
            target: options.file().unwrap().clone(),
            bytes: b"rendered image".to_vec(),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(original.join("plan.svg")).unwrap(),
            b"rendered image"
        );
        assert!(!replacement.join("plan.svg").exists());
    }

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

    #[test]
    fn theme_resolution_uses_output_specific_defaults_and_explicit_values() {
        let svg = PlanImageTarget::try_from(PathBuf::from("plan.svg")).unwrap();
        let png = PlanImageTarget::try_from(PathBuf::from("plan.png")).unwrap();

        let cases = [
            (
                Some(&svg),
                false,
                None,
                Some(ResolvedFileTheme::Svg(SvgTheme::Fixed(Theme::Light))),
                None,
            ),
            (
                Some(&png),
                false,
                None,
                Some(ResolvedFileTheme::Png(Theme::Light)),
                None,
            ),
            (
                Some(&svg),
                true,
                None,
                Some(ResolvedFileTheme::Svg(SvgTheme::Fixed(Theme::Light))),
                Some(Theme::Dark),
            ),
            (
                Some(&png),
                true,
                None,
                Some(ResolvedFileTheme::Png(Theme::Light)),
                Some(Theme::Dark),
            ),
            (None, false, None, None, None),
            (None, true, None, None, Some(Theme::Dark)),
            (
                Some(&svg),
                true,
                Some(ImageTheme::Light),
                Some(ResolvedFileTheme::Svg(SvgTheme::Fixed(Theme::Light))),
                Some(Theme::Light),
            ),
            (
                Some(&png),
                true,
                Some(ImageTheme::Dark),
                Some(ResolvedFileTheme::Png(Theme::Dark)),
                Some(Theme::Dark),
            ),
            (
                Some(&svg),
                true,
                Some(ImageTheme::Auto),
                Some(ResolvedFileTheme::Svg(SvgTheme::Auto)),
                Some(Theme::Dark),
            ),
        ];

        for (target, inline, requested, expected_file, expected_inline) in cases {
            let resolved = resolve_image_themes(target, inline, requested, || Theme::Dark).unwrap();
            assert_eq!(resolved.file, expected_file);
            assert_eq!(resolved.inline, expected_inline);
        }
    }

    #[test]
    fn fixed_inline_theme_does_not_query_the_terminal_background() {
        let resolved = resolve_image_themes(None, true, Some(ImageTheme::Dark), || {
            panic!("fixed themes must not query the terminal background")
        })
        .unwrap();

        assert_eq!(resolved.inline, Some(Theme::Dark));
    }

    #[test]
    fn explicit_png_auto_theme_is_rejected_before_rendering() {
        let png = PlanImageTarget::try_from(PathBuf::from("plan.png")).unwrap();
        let error = resolve_image_themes(Some(&png), false, Some(ImageTheme::Auto), || {
            panic!("PNG auto validation must happen before terminal theme detection")
        })
        .unwrap_err();

        assert!(matches!(error, PlanImageError::PngAutoTheme));
        assert!(error.to_string().contains("--image-theme auto"));
        assert!(error.to_string().contains("PNG"));
    }
}
