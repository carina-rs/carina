//! Schema-only, directory-shaped fixtures shared with the LSP regression.

use std::path::{Path, PathBuf};

use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderFactory,
    ProviderInitialization, ProviderResult, ProviderSession, ProviderValidationInputs,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema, TypeIdentity,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};

pub const SOURCE: &str = "let src = test.Thing { name = 'src' }\n";
pub const ENVIRONMENT: &str = "'dev' | 'prod'";

#[derive(Clone, Copy)]
pub enum Scope {
    Root,
    Module,
}

pub struct Directory {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    scope: Scope,
}

impl Directory {
    pub fn new(scope: Scope) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let templates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("carina-cli/tests/fixtures/fn_type_promise");
        let root = temporary.path().join("project");
        let template = match scope {
            Scope::Root => "root",
            Scope::Module => "module",
        };
        copy_directory(&templates.join(template), &root);
        copy_directory(
            &templates.join("upstream"),
            &temporary.path().join("upstream"),
        );
        Self {
            _temporary: temporary,
            root,
            scope,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source_file(&self) -> PathBuf {
        match self.scope {
            Scope::Root => self.root.join("main.crn"),
            Scope::Module => self.root.join("child/main.crn"),
        }
    }

    pub fn write_source(&self, source: &str) {
        std::fs::write(self.source_file(), source).unwrap();
    }

    pub fn upstream_binding(&self) -> &'static str {
        match self.scope {
            Scope::Root => "let upstream = upstream_state { source = '../upstream' }\n",
            Scope::Module => "",
        }
    }

    pub fn argument_expression(&self, expression: &str, untyped_type: &str) -> String {
        let Scope::Module = self.scope else {
            return expression.to_owned();
        };
        let Some(export) = expression.strip_prefix("upstream.") else {
            return expression.to_owned();
        };
        let ty = match export {
            "typed" => "String",
            "count" => "Int",
            "named" => "test.Name",
            "items" => "list(String)",
            "untyped" if matches!(untyped_type, "Int" | "list(String)") => untyped_type,
            "untyped" => "String",
            _ => panic!("unknown fixture export"),
        };
        // Modules cannot declare upstream_state. Pass the root's expression
        // through a module argument; its original type evidence must survive.
        std::fs::write(
            self.root.join("main.crn"),
            format!(
                "let upstream = upstream_state {{ source = '../upstream' }}\nlet child = use {{ source = './child' }}\nchild {{ upstream_value = {expression} }}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            self.root.join("child/upstream-argument.crn"),
            format!("arguments {{ upstream_value: {ty} }}\n"),
        )
        .unwrap();
        "upstream_value".into()
    }
}

fn copy_directory(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// Ported from the old fn_declared_checks fixture, retaining only schemas.
// Validation must never initialize or execute this provider.
pub struct PromiseFactory;

impl ProviderFactory for PromiseFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "Function type promise test provider"
    }

    fn validate_config(
        &self,
        _: &CanonicalProviderConfigAttributes,
        _: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _: &ProviderSession,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        panic!("validate must use schemas without initializing the provider")
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("Thing")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("identifier", AttributeType::string())
                        .build(),
                )
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("count", AttributeType::int()).build(),
                )
                .attribute(
                    AttributeSchema::provider_populated(
                        "items",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::provider_populated("typed_name", AttributeType::ref_("Name"))
                        .build(),
                )
                .with_def(
                    "Name",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("test.Name")),
                        Some(PatternSpec::new("allowed", PatternMatch::Full)),
                        None,
                        None,
                    ),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Lookup")
                .as_data_source()
                .attribute(AttributeSchema::required("query", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("value", AttributeType::string()).build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::required("name", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("value", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("number", AttributeType::int()).build())
                .attribute(
                    AttributeSchema::optional(
                        "items",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        ]
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}
