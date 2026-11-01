use std::path::PathBuf;

use wit_parser::{Resolve, Type, TypeDefKind};

#[test]
fn create_progress_requires_an_identifier_for_read_back() {
    let mut resolve = Resolve::default();
    let wit = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../carina-plugin-wit/wit");
    let (package, _) = resolve.push_dir(wit).expect("provider WIT must parse");
    let types = resolve.packages[package].interfaces["types"];
    let types = &resolve.interfaces[types].types;
    let progress = *types
        .get("create-progress")
        .expect("create/resume progress must couple the identifier with the phase");
    let TypeDefKind::Variant(variant) = &resolve.types[progress].kind else {
        panic!("create progress must be a variant")
    };
    assert_eq!(variant.cases.len(), 2);
    assert_eq!(variant.cases[0].name, "operating");
    let Some(Type::Id(identifier)) = variant.cases[0].ty else {
        panic!("operation progress must carry an optional identifier")
    };
    assert!(matches!(
        resolve.types[identifier].kind,
        TypeDefKind::Option(Type::String)
    ));
    assert_eq!(variant.cases[1].name, "reading-back");
    assert_eq!(variant.cases[1].ty, Some(Type::String));

    for name in ["pending-create", "resume-pending"] {
        let TypeDefKind::Record(record) = &resolve.types[types[name]].kind else {
            panic!("{name} must be a record")
        };
        assert!(
            record
                .fields
                .iter()
                .any(|field| { field.name == "progress" && field.ty == Type::Id(progress) })
        );
        assert!(
            record
                .fields
                .iter()
                .all(|field| field.name != "identifier" && field.name != "phase")
        );
    }
    let TypeDefKind::Record(operation) = &resolve.types[types["pending-operation"]].kind else {
        panic!("pending operation must be a record")
    };
    assert!(operation.fields.iter().all(|field| field.name != "phase"));
    let TypeDefKind::Record(update) = &resolve.types[types["pending-update"]].kind else {
        panic!("pending update must be a record")
    };
    assert!(update.fields.iter().any(|field| field.name == "phase"));
}
