use carina_core::resource::{ConcreteValue, Value};
use carina_state::{PublishedExports, StateFile, check_and_migrate_bytes};

fn finite(value: f64) -> carina_core::FiniteF64 {
    carina_core::FiniteF64::new(value).unwrap()
}

fn state_with_export_json(value: &str) -> Vec<u8> {
    let mut json = serde_json::to_value(StateFile::new()).unwrap();
    json["exports"] = serde_json::json!({"ratio":"RAW_FLOAT"});
    serde_json::to_string(&json)
        .unwrap()
        .replace("\"RAW_FLOAT\"", value)
        .into_bytes()
}

#[test]
fn overflowing_state_and_published_exports_fail_before_bindings_are_built() {
    for raw in ["1e999", "-1e999", "[1e999]", r#"{"nested":-1e999}"#] {
        let bytes = state_with_export_json(raw);
        assert!(check_and_migrate_bytes(&bytes).is_err());
        assert!(PublishedExports::from_state_bytes(&bytes).is_err());
    }
}

#[test]
fn finite_published_exports_preserve_signed_zero_when_loading_bindings() {
    for raw in [-0.0, 0.0, f64::MAX] {
        let json = serde_json::to_string(&finite(raw)).unwrap();
        let bytes = state_with_export_json(&json);
        let loaded = PublishedExports::from_state_bytes(&bytes).unwrap();
        let bindings = loaded.build_remote_bindings();
        let Value::Concrete(ConcreteValue::Float(value)) = bindings["ratio"] else {
            panic!("expected float")
        };
        assert_eq!(value.get().to_bits(), raw.to_bits());
        assert!(check_and_migrate_bytes(&bytes).is_ok());
    }
}
