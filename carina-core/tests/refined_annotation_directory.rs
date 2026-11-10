#[path = "support/refined_annotations.rs"]
mod fixture;

#[test]
fn read_values_ignore_slot_members_and_distinct_identities_stay_heterogeneous() {
    let snapshot = fixture::snapshot();
    for &(main, exports) in fixture::READ_CASES {
        let dir = fixture::directory(main, exports);
        fixture::validate(dir.path(), &snapshot)
            .unwrap_or_else(|error| panic!("{main}\n{exports}\n{error}"));
        let mixed = exports.replace("b.account_id", "a.other_id");
        let dir = fixture::directory(main, &mixed);
        let error = fixture::validate(dir.path(), &snapshot)
            .expect_err("different identities require an explicit compatible sink");
        assert!(
            error.contains("aws.AccountId") && error.contains("aws.OtherId"),
            "{error}"
        );
    }
}

#[test]
fn member_refined_account_ids_satisfy_every_directory_annotation() {
    let snapshot = fixture::snapshot();
    for &(main, exports) in fixture::CASES {
        let dir = fixture::directory(main, exports);
        fixture::validate(dir.path(), &snapshot)
            .unwrap_or_else(|error| panic!("{main}\n{exports}\n{error}"));
    }
}

#[test]
fn directory_annotations_reject_unrelated_identities() {
    let snapshot = fixture::snapshot();
    for &(main, exports) in fixture::CASES {
        let dir = fixture::directory(
            &main.replace("registry_prod.account_id", "registry_prod.other_id"),
            &exports.replace("registry_prod.account_id", "registry_prod.other_id"),
        );
        let error = fixture::validate(dir.path(), &snapshot).expect_err(&format!(
            "unrelated identity cannot satisfy: {main}\n{exports}"
        ));
        assert!(
            error.contains("aws.AccountId") && error.contains("aws.OtherId"),
            "{error}"
        );
        assert!(
            !error.contains("expected aws.AccountId, got aws.AccountId"),
            "{error}"
        );
    }
}

#[test]
fn round14_declared_directory_binding_discharges_members_once() {
    let loaded = fixture::snapshot();
    for length in [5, 50] {
        let main = format!(
            "fn make_name(): aws.Name {{ '{}' }}\nlet named = make_name()\nlet sink = awscc.MemberSink {{ name = named }}\n",
            "A".repeat(length)
        );
        let dir = fixture::directory(&main, "exports {}\n");
        let result = fixture::validate(dir.path(), &loaded);
        assert_eq!(result.is_ok(), length == 5, "{main}\n{result:?}");
        if let Err(error) = result {
            assert_eq!(error.matches("outside allowed range").count(), 1, "{error}");
        }
    }
}

#[path = "support/refined_destinations.rs"]
mod destinations;

macro_rules! boundary_test {
    ($name:ident, $boundary:ident) => {
        #[test]
        fn $name() {
            let snapshot = fixture::snapshot();
            let boundary = destinations::Boundary::$boundary;
            for length in [None, Some(5), Some(50)] {
                let result = destinations::check(boundary, &snapshot, length);
                assert_eq!(
                    result.is_ok(),
                    length != Some(50),
                    "{boundary:?} {length:?}: {result:?}"
                );
                if let Err(error) = result {
                    assert_eq!(
                        error.matches("outside allowed range").count(),
                        1,
                        "{boundary:?}: {error}"
                    );
                }
            }
        }
    };
}
boundary_test!(round15_resource_attribute, Resource);
boundary_test!(round15_let_forwarding, Let);
boundary_test!(round15_export, Export);
boundary_test!(round15_module_argument, ModuleArgument);
boundary_test!(round15_function_parameter, FunctionParameter);
boundary_test!(round15_function_return, FunctionReturn);
boundary_test!(round15_attributes_block, Attributes);
