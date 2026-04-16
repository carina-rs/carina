//! Render plan output from a `tests/fixtures/plan_display` fixture.
//!
//! Mirrors `carina plan --refresh=false` but skips provider plugin loading,
//! so this works without `carina init` or cached provider binaries. Used by
//! the `make plan-*` targets for visual confirmation of plan display.
//!
//! Usage:
//!   cargo run --example plan-fixture -- <fixture_name> [--detail <level>] [--destroy] [--tui]
//!
//! Flags:
//!   --detail <full|explicit|none>   Detail level (default: full)
//!   --destroy                       Render as destroy plan (uses state's live resources)
//!   --tui                           Launch interactive TUI instead of printing

use std::process::ExitCode;

use carina_cli::DetailLevel;
use carina_cli::display::{format_destroy_plan, print_plan};
use carina_cli::fixture_plan::{
    build_plan_from_fixture_name, delete_attributes_from_plan, delete_attributes_from_states,
};

fn print_usage() {
    eprintln!(
        "usage: plan-fixture <fixture_name> [--detail <full|explicit|none>] [--destroy] [--tui]"
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

    let fp = build_plan_from_fixture_name(&fixture);

    if tui {
        if let Err(e) = carina_tui::run(&fp.plan, &fp.schemas) {
            eprintln!("TUI error: {}", e);
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    if destroy {
        let delete_attributes = delete_attributes_from_states(&fp.current_states);
        print!(
            "{}",
            format_destroy_plan(&fp.plan, detail, &delete_attributes)
        );
    } else {
        let delete_attributes = delete_attributes_from_plan(&fp.plan, &fp.current_states);
        print_plan(
            &fp.plan,
            detail,
            &delete_attributes,
            Some(&fp.schemas),
            &fp.moved_origins,
            &[],
            &fp.deferred_for_expressions,
        );
    }

    ExitCode::SUCCESS
}
