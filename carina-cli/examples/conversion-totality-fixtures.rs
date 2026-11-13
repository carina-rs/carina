//! Regenerate the no-`as` matrix, or validate a twin using the same native
//! schemas on either branch. No provider CRUD is invoked by this helper.
#[path = "../tests/support/conversion_totality.rs"]
mod round5_fixtures;
#[path = "../tests/support/conversion_twins.rs"]
mod twins;

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let command = arguments
        .next()
        .expect("--emit <directory> | --validate <directory>");
    let directory = arguments.next().expect("fixture directory");
    assert!(arguments.next().is_none(), "unexpected argument");
    if command == "--emit" {
        let count = twins::emit_twins(std::path::Path::new(&directory));
        println!(
            "{count} no-as twins: {}",
            std::path::Path::new(&directory).display()
        );
    } else if command == "--validate" {
        let result = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(&directory).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(round5_fixtures::factories()),
            Default::default(),
        );
        println!("{}", result.json);
    } else {
        panic!("--emit <directory> | --validate <directory>");
    }
}
