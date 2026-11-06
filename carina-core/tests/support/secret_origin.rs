use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedResourceSchema,
};

pub const MAIN: &str = "mock.test.Secret { params = secret(\"private-${producer.text}\") }\n";

pub fn schema() -> ValidatedResourceSchema {
    ResourceSchema::new("test.Secret")
        .attribute(AttributeSchema::optional("text", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("params", AttributeType::int()).build())
        .validate()
        .unwrap()
}

pub fn write_directory(path: &std::path::Path) {
    for (name, source) in [
        ("providers.crn", "provider mock {}\n"),
        ("producer.crn", "let producer = mock.test.Secret {}\n"),
        ("main.crn", MAIN),
    ] {
        std::fs::write(path.join(name), source).unwrap();
    }
}

pub fn reference_schema() -> ValidatedResourceSchema {
    ResourceSchema::new("test.Secret")
        .attribute(AttributeSchema::optional("params", AttributeType::int()).build())
        .attribute(
            AttributeSchema::optional(
                "config",
                AttributeType::struct_(
                    "Config",
                    vec![
                        carina_core::schema::StructField::optional(
                            "known",
                            AttributeType::string(),
                        )
                        .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap()
}
