//! Render plan output from a `tests/fixtures/plan_display` fixture.
//!
//! Mirrors `carina plan --refresh=false` but skips provider plugin loading,
//! so this works without `carina init` or cached provider binaries. Used by
//! the `make plan-*` targets for visual confirmation of plan display.
//!
//! Usage:
//!   cargo run --example plan-fixture -- <fixture_name> [--detail <level>] [--destroy] [--tui] [--image <path>] [--image-inline] [--image-theme <light|dark|auto>]
//!
//! Flags:
//!   --detail <full|explicit|none>   Detail level (default: full)
//!   --destroy                       Render as destroy plan (uses state's live resources)
//!   --tui                           Launch interactive TUI instead of printing
//!   --image <path>                  Also write the plan graph as SVG or PNG
//!   --image-inline                  Display the plan graph in the terminal
//!   --image-theme <light|dark|auto> Select the image palette

use std::io::IsTerminal;
use std::process::ExitCode;

use carina_cli::DetailLevel;
use carina_cli::display::{format_destroy_plan_with_delete_instances, print_plan};
use carina_cli::fixture_plan::{
    build_fixture_plan_change_summary, build_plan_from_fixture_name_using,
    delete_instance_attributes_from_plan,
};
use carina_cli::plan_image::{
    ImageTheme, PlanImageInput, PlanImageTarget, StateMetadata, parse_output_target,
    render_plan_outputs, resolve_plan_image_options, write_inline_image, write_plan_image,
};
use carina_cli::wiring::LoadedProviderFactory;

fn print_usage() {
    eprintln!(
        "usage: plan-fixture <fixture_name> [--detail <full|explicit|none>] [--destroy] [--tui] [--image <path>] [--image-inline] [--image-theme <light|dark|auto>]"
    );
}

fn parse_detail(s: &str) -> Option<DetailLevel> {
    match s {
        "full" => Some(DetailLevel::Full),
        "explicit" => Some(DetailLevel::Explicit),
        "none" => Some(DetailLevel::None),
        _ => None,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }

    let mut fixture: Option<String> = None;
    let mut detail = DetailLevel::Full;
    let mut destroy = false;
    let mut tui = false;
    let mut image: Option<PlanImageTarget> = None;
    let mut image_inline = false;
    let mut image_theme = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--detail" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    eprintln!("error: --detail requires a value");
                    return ExitCode::from(2);
                };
                let Some(parsed) = parse_detail(value) else {
                    eprintln!("error: unknown --detail value: {}", value);
                    return ExitCode::from(2);
                };
                detail = parsed;
            }
            "--destroy" => destroy = true,
            "--tui" => tui = true,
            "--image-inline" => image_inline = true,
            "--image-theme" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    eprintln!("error: --image-theme requires a value");
                    return ExitCode::from(2);
                };
                image_theme = match value.as_str() {
                    "light" => Some(ImageTheme::Light),
                    "dark" => Some(ImageTheme::Dark),
                    "auto" => Some(ImageTheme::Auto),
                    _ => {
                        eprintln!("error: unknown --image-theme value: {value}");
                        return ExitCode::from(2);
                    }
                };
            }
            "--image" => {
                i += 1;
                let Some(value) = args.get(i) else {
                    eprintln!("error: --image requires a value");
                    return ExitCode::from(2);
                };
                match parse_output_target(value) {
                    Ok(target) => image = Some(target),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return ExitCode::from(2);
                    }
                }
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other if !other.starts_with("--") && fixture.is_none() => {
                fixture = Some(other.to_string());
            }
            other => {
                eprintln!("error: unexpected argument: {}", other);
                print_usage();
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let Some(fixture) = fixture else {
        print_usage();
        return ExitCode::from(2);
    };

    if image_inline && tui {
        eprintln!("error: --image-inline cannot be used with --tui");
        return ExitCode::from(2);
    }

    let images = match resolve_plan_image_options(
        image.as_ref(),
        image_inline,
        image_theme,
        |name| std::env::var(name).ok(),
        std::io::stdout().is_terminal(),
    ) {
        Ok(images) => images,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
    };

    let fp = build_plan_from_fixture_name_using(&fixture, LoadedProviderFactory::for_test);
    let historical_display =
        delete_instance_attributes_from_plan(&fp.plan, &fp.managed_current, fp.state_file.as_ref());
    let summary = build_fixture_plan_change_summary(&fp);

    let (rendered_image, rendered_inline_image) = if !images.is_empty() {
        let state = fp.state_file.as_ref().map(|state| StateMetadata {
            lineage: &state.lineage,
            serial: state.serial,
        });
        match render_plan_outputs(
            images.file(),
            images.inline(),
            PlanImageInput {
                plan: &fp.plan,
                desired_resources: &fp.desired_resources,
                unresolved_resources: &fp.unresolved_resources,
                compositions: &fp.compositions,
                expansion_trace: Some(&fp.expansion_trace),
                state,
                summary: &summary,
            },
        ) {
            Ok(rendered) => rendered,
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::from(1);
            }
        }
    } else {
        (None, None)
    };

    if let Some(rendered_image) = rendered_image {
        if let Err(error) = write_plan_image(rendered_image) {
            eprintln!("error: {error}");
            return ExitCode::from(1);
        }
        eprintln!(
            "Plan image written to {}",
            images
                .file()
                .expect("a rendered file image has a target")
                .path()
                .display()
        );
    }

    if tui {
        if let Err(e) = carina_tui::run(
            &fp.plan,
            &fp.schemas,
            &historical_display,
            Some(&fp.prev_explicit),
        ) {
            eprintln!("TUI error: {}", e);
            return ExitCode::from(1);
        }
        fp.moved_hints.eprint();
        return ExitCode::SUCCESS;
    }

    if destroy {
        print!(
            "{}",
            format_destroy_plan_with_delete_instances(&fp.plan, detail, &historical_display)
        );
    } else {
        print_plan(
            &fp.plan,
            &summary,
            detail,
            &historical_display,
            Some(&fp.schemas),
            &fp.moved_origins,
            Some(&fp.prev_explicit),
            None,
        );
    }
    fp.moved_hints.eprint();

    if let Some(rendered_inline_image) = rendered_inline_image
        && let Err(error) = write_inline_image(rendered_inline_image, std::io::stdout().lock())
    {
        eprintln!("error: {error}");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}
