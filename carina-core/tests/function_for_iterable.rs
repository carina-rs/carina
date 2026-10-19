use carina_core::parser::{ProviderContext, parse};

#[test]
fn dotted_ref_function_result_can_drive_a_for_iterable_during_bootstrap_parse() {
    let parsed = parse(
        r#"
fn names(r: mock.compute.Instance): list(String) { ["a", "b"] }
let vpc = mock.compute.Instance { name = "vpc" }
for n in names(vpc) {
  mock.test.resource { name = n }
}
"#,
        &ProviderContext::default(),
    )
    .expect("the body is evaluable before the Ref annotation is schema-checked");

    assert_eq!(parsed.resources.len(), 3);
}
