use super::layout::Layout;
use super::model::GraphModel;
use super::svg::{SvgDocument, render_svg};
use super::{PlanImageError, SvgTheme, Theme};

const JETBRAINS_MONO_REGULAR: &[u8] =
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf");
const JETBRAINS_MONO_BOLD: &[u8] = include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf");
const MAX_PNG_PIXELS: u64 = 64_000_000;

pub(crate) fn render_png(
    model: &GraphModel,
    layout: &Layout,
    theme: Theme,
) -> Result<Vec<u8>, PlanImageError> {
    let svg = render_svg(model, layout, SvgTheme::Fixed(theme));
    rasterize_svg(&svg)
}

fn rasterize_svg(svg: &SvgDocument) -> Result<Vec<u8>, PlanImageError> {
    let scaled_width = u64::from(svg.width) * 2;
    let scaled_height = u64::from(svg.height) * 2;
    let pixels = scaled_width.saturating_mul(scaled_height);
    if pixels > MAX_PNG_PIXELS {
        return Err(PlanImageError::PngPixelBudget {
            width: scaled_width,
            height: scaled_height,
            pixels,
            max_pixels: MAX_PNG_PIXELS,
        });
    }
    let width = u32::try_from(scaled_width).map_err(|_| PlanImageError::PngDimensions)?;
    let height = u32::try_from(scaled_height).map_err(|_| PlanImageError::PngDimensions)?;

    let mut options = resvg::usvg::Options {
        font_family: "JetBrains Mono".to_string(),
        ..resvg::usvg::Options::default()
    };
    let fontdb = options.fontdb_mut();
    fontdb.load_font_data(JETBRAINS_MONO_REGULAR.to_vec());
    fontdb.load_font_data(JETBRAINS_MONO_BOLD.to_vec());
    fontdb.set_monospace_family("JetBrains Mono");

    let tree = resvg::usvg::Tree::from_str(&svg.text, &options)
        .map_err(|error| PlanImageError::SvgParse { error })?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(PlanImageError::PngDimensions)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(2.0, 2.0),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| PlanImageError::PngEncoding {
            error: Box::new(error),
        })
}

#[cfg(test)]
mod tests {
    use crate::plan_image::Theme;
    use crate::plan_image::layout::layout_graph;
    use crate::plan_image::model::{PlanImageInput, build_model};

    use super::*;

    #[test]
    fn png_is_valid_and_exactly_twice_the_svg_dimensions() {
        let runtime = crate::test_support::TestCommandRuntime::new();

        let fixture =
            crate::fixture_plan::build_plan_from_fixture_name(&runtime, "mixed_operations")
                .expect("fixture mock inputs must be valid");
        let summary = crate::fixture_plan::build_fixture_plan_change_summary(&fixture);
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: None,
            summary: &summary,
        });
        let layout = layout_graph(&model);

        let png = render_png(&model, &layout, Theme::Light).unwrap();

        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
        assert_eq!(width, layout.width * 2);
        assert_eq!(height, layout.height * 2);
    }

    #[test]
    fn png_rejects_dimensions_over_the_pixel_budget_and_suggests_svg() {
        let svg = SvgDocument {
            text: r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#.to_string(),
            width: 4_001,
            height: 4_000,
        };

        let error =
            rasterize_svg(&svg).expect_err("64Mpx budget must be enforced before allocation");

        assert!(matches!(
            error,
            PlanImageError::PngPixelBudget {
                width: 8_002,
                height: 8_000,
                pixels: 64_016_000,
                max_pixels: 64_000_000,
            }
        ));
        assert!(error.to_string().contains(".svg"));
    }
}
