//! Fixture generator uses main's public schema APIs so its no-`as` twins can
//! also be validated by the main branch's binary with the same native factory.
use carina_core::{provider::*, schema::*};
use std::{collections::BTreeMap, path::Path};

pub struct TotalityFactory;

impl ProviderFactory for TotalityFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Mock totality fixture"
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
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
        let provider = carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        )
        .create_provider();
        Box::pin(
            async move { Ok(InitializeOutcome::unavailable().with_provider(Box::new(provider))) },
        )
    }
    fn create_normalizer(
        &self,
        _: &ProviderSession,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let name = || {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Name")),
                None,
                Some((Some(1), Some(8))),
                None,
            )
        };
        let mut schemas = carina_provider_mock::builtin_schemas();
        schemas.extend([
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("attr", name()).build())
                .attribute(AttributeSchema::optional("items", AttributeType::list(name())).build())
                .attribute(AttributeSchema::optional("values", AttributeType::map(name())).build())
                .attribute(
                    AttributeSchema::optional(
                        "config",
                        AttributeType::struct_(
                            "Config",
                            vec![StructField::optional("value", name()).build()],
                        ),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::optional(
                        "choice",
                        AttributeType::union(vec![name(), AttributeType::string()]),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Source")
                .attribute(
                    AttributeSchema::optional(
                        "items",
                        AttributeType::list(AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("Query")
                .as_data_source()
                .attribute(AttributeSchema::optional("attr", name()).build())
                .validate()
                .unwrap(),
            carina_provider_mock::unique_list_schema()
                .validate()
                .unwrap(),
        ]);
        schemas
    }
}

pub fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(TotalityFactory)]
}

#[derive(Clone, Copy, Debug)]
pub enum Position {
    Root,
    Parens,
    ListItem,
    MapValue,
    StructField,
    IfBranch,
    Coalesce,
    Alias,
    Union,
    Secret,
    DataSource,
}
pub const POSITIONS: [Position; 11] = [
    Position::Root,
    Position::Parens,
    Position::ListItem,
    Position::MapValue,
    Position::StructField,
    Position::IfBranch,
    Position::Coalesce,
    Position::Alias,
    Position::Union,
    Position::Secret,
    Position::DataSource,
];
impl Position {
    pub fn label(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Parens => "parens",
            Self::ListItem => "list-item",
            Self::MapValue => "map-value",
            Self::StructField => "struct-field",
            Self::IfBranch => "if-branch",
            Self::Coalesce => "coalesce",
            Self::Alias => "alias",
            Self::Union => "union",
            Self::Secret => "secret-operand",
            Self::DataSource => "data-source",
        }
    }
    fn body(self, quality: Quality, converted: bool, unknown: bool) -> String {
        let value = match quality {
            Quality::Valid => "'ok'",
            Quality::Invalid => "42",
        };
        let convert = |operand: &str| {
            if converted {
                format!("{operand} as mock.Name")
            } else {
                operand.to_owned()
            }
        };
        let expression = convert(value);
        let (prefix, attribute) = match self {
            Self::Root | Self::DataSource => (String::new(), format!("attr = {expression}")),
            Self::Parens => (String::new(), format!("attr = (({expression}))")),
            Self::ListItem => (String::new(), format!("items = [{expression}]")),
            Self::MapValue => (String::new(), format!("values = {{ key = {expression} }}")),
            Self::StructField => (
                String::new(),
                format!("config = {{ value = {expression} }}"),
            ),
            Self::IfBranch => (
                String::new(),
                format!(
                    "attr = if true {{ {expression} }} else {{ {} }}",
                    convert("'ok'")
                ),
            ),
            Self::Coalesce => (
                String::new(),
                format!("attr = ({expression}) ?? ({})", convert("'ok'")),
            ),
            Self::Alias => (
                format!("let converted = {expression}\n"),
                "attr = converted".into(),
            ),
            Self::Union => (String::new(), format!("choice = {expression}")),
            Self::Secret => (
                String::new(),
                format!("attr = {}", convert(&format!("(secret({value}))"))),
            ),
        };
        let resource = if unknown {
            "mock.NotLoaded"
        } else {
            match self {
                Self::DataSource => "read mock.Query",
                Self::Root
                | Self::Parens
                | Self::ListItem
                | Self::MapValue
                | Self::StructField
                | Self::IfBranch
                | Self::Coalesce
                | Self::Alias
                | Self::Union
                | Self::Secret => "mock.Sink",
            }
        };
        let prefix = if matches!(self, Self::DataSource) && !unknown {
            format!("{prefix}let query = ")
        } else {
            prefix
        };
        format!("{prefix}{resource} {{ {attribute} }}\n")
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Liveness {
    Live,
    EmptyListFor,
    EmptyIndexedFor,
    EmptyMapFor,
    UnselectedIf,
    DeferredFor,
    UnselectedModuleIf,
    ModuleTemplate,
    ModuleCallNotExpanded,
    ModuleInstance,
    DestinationUnknown,
    OrdinaryStopped,
    ReadOnly,
}
pub const LIVENESS: [Liveness; 13] = [
    Liveness::Live,
    Liveness::EmptyListFor,
    Liveness::EmptyIndexedFor,
    Liveness::EmptyMapFor,
    Liveness::UnselectedIf,
    Liveness::DeferredFor,
    Liveness::UnselectedModuleIf,
    Liveness::ModuleTemplate,
    Liveness::ModuleCallNotExpanded,
    Liveness::ModuleInstance,
    Liveness::DestinationUnknown,
    Liveness::OrdinaryStopped,
    Liveness::ReadOnly,
];
impl Liveness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::EmptyListFor => "empty-list-for",
            Self::EmptyIndexedFor => "empty-indexed-for",
            Self::EmptyMapFor => "empty-map-for",
            Self::UnselectedIf => "unselected-if",
            Self::DeferredFor => "deferred-for",
            Self::UnselectedModuleIf => "unselected-module-if",
            Self::ModuleTemplate => "module-template",
            Self::ModuleCallNotExpanded => "module-call-not-expanded",
            Self::ModuleInstance => "module-instance",
            Self::DestinationUnknown => "destination-unknown",
            Self::OrdinaryStopped => "ordinary-stopped",
            Self::ReadOnly => "read-only",
        }
    }
    pub fn has_live_destination(self) -> bool {
        match self {
            Self::Live | Self::ModuleInstance => true,
            Self::EmptyListFor
            | Self::EmptyIndexedFor
            | Self::EmptyMapFor
            | Self::UnselectedIf
            | Self::DeferredFor
            | Self::UnselectedModuleIf
            | Self::ModuleTemplate
            | Self::ModuleCallNotExpanded
            | Self::DestinationUnknown
            | Self::OrdinaryStopped
            | Self::ReadOnly => false,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Quality {
    Valid,
    Invalid,
}
pub const QUALITIES: [Quality; 2] = [Quality::Valid, Quality::Invalid];
impl Quality {
    pub fn label(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
        }
    }
}

pub fn resource_cell_exclusion(position: Position, liveness: Liveness) -> Option<&'static str> {
    if matches!(position, Position::DataSource) && matches!(liveness, Liveness::DeferredFor) {
        Some(
            "main requires a read-producing loop's iterable before module expansion; an apply-deferred read body cannot exist",
        )
    } else {
        None
    }
}

#[derive(Debug)]
pub struct Fixture {
    pub files: BTreeMap<String, String>,
}
impl Fixture {
    pub fn write(&self, directory: &Path) {
        for (file, source) in &self.files {
            let path = directory.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    FunctionBody,
    CalledBody,
    ParameterDefault,
    SelectedDefault,
    OverriddenDefault,
    ModuleDefault,
    OverriddenModuleDefault,
    Output,
    Export,
    ProviderConfig,
}
pub const BOUNDARIES: [Boundary; 10] = [
    Boundary::FunctionBody,
    Boundary::CalledBody,
    Boundary::ParameterDefault,
    Boundary::SelectedDefault,
    Boundary::OverriddenDefault,
    Boundary::ModuleDefault,
    Boundary::OverriddenModuleDefault,
    Boundary::Output,
    Boundary::Export,
    Boundary::ProviderConfig,
];
impl Boundary {
    pub fn label(self) -> &'static str {
        match self {
            Self::FunctionBody => "function-body",
            Self::CalledBody => "called-body",
            Self::ParameterDefault => "parameter-default",
            Self::SelectedDefault => "selected-default",
            Self::OverriddenDefault => "overridden-default",
            Self::ModuleDefault => "module-default",
            Self::OverriddenModuleDefault => "overridden-module-default",
            Self::Output => "output",
            Self::Export => "export",
            Self::ProviderConfig => "provider-config",
        }
    }
}

pub fn boundary_fixture(
    boundary: Boundary,
    liveness: Liveness,
    quality: Quality,
    converted: bool,
) -> Result<Fixture, &'static str> {
    match liveness {
        Liveness::EmptyListFor
        | Liveness::EmptyIndexedFor
        | Liveness::EmptyMapFor
        | Liveness::UnselectedIf
        | Liveness::DeferredFor
        | Liveness::UnselectedModuleIf => {
            return Err(
                "structural bodies accept local lets followed by one resource/read/module expression; boundary declarations cannot be that tail",
            );
        }
        Liveness::DestinationUnknown => {
            return Err(
                "schema-None is a resource/data-source use decision, not a boundary declaration",
            );
        }
        Liveness::Live
        | Liveness::ModuleTemplate
        | Liveness::ModuleCallNotExpanded
        | Liveness::ModuleInstance
        | Liveness::OrdinaryStopped
        | Liveness::ReadOnly => {}
    }
    let module = matches!(
        liveness,
        Liveness::ModuleTemplate | Liveness::ModuleCallNotExpanded | Liveness::ModuleInstance
    );
    if module && matches!(boundary, Boundary::Export | Boundary::ProviderConfig) {
        return Err(
            "main forbids exports/providers in modules and stops before resolver template B exists",
        );
    }
    if matches!(liveness, Liveness::OrdinaryStopped)
        && matches!(
            boundary,
            Boundary::ModuleDefault | Boundary::OverriddenModuleDefault
        )
    {
        return Err("the ordinary root failure stops main before resolver template B exists");
    }
    let mut generated = fixture(Position::Root, liveness, Quality::Valid, false);
    let value = match quality {
        Quality::Valid => "'ok'",
        Quality::Invalid => "'longer-than-eight'",
    };
    let expression = if converted {
        format!("{value} as mock.Name")
    } else {
        value.into()
    };
    let declaration = match boundary {
        Boundary::FunctionBody | Boundary::CalledBody => format!(
            "fn supply(): String {{ {expression} }}\n{}",
            if matches!(boundary, Boundary::CalledBody) {
                "let supplied = supply()\n"
            } else {
                ""
            }
        ),
        Boundary::ParameterDefault | Boundary::SelectedDefault | Boundary::OverriddenDefault => {
            format!(
                "fn supply(value: String = {expression}): String {{ value }}\n{}",
                match boundary {
                    Boundary::ParameterDefault => "",
                    Boundary::SelectedDefault => "let supplied = supply()\n",
                    Boundary::OverriddenDefault => "let supplied = supply('ok')\n",
                    Boundary::FunctionBody
                    | Boundary::CalledBody
                    | Boundary::ModuleDefault
                    | Boundary::OverriddenModuleDefault
                    | Boundary::Output
                    | Boundary::Export
                    | Boundary::ProviderConfig => unreachable!("outer arm fixes default boundary"),
                }
            )
        }
        Boundary::ModuleDefault | Boundary::OverriddenModuleDefault => {
            let file = generated
                .files
                .entry("child/main.crn".into())
                .or_insert_with(|| "mock.Sink { attr = 'ok' }\n".into());
            *file = format!("arguments {{ value: String = {expression} }}\n{file}");
            if !module {
                generated.files.insert(
                    "main.crn".into(),
                    format!(
                        "let child = use {{ source = './child' }}\nlet net = child {{ {} }}\n",
                        if matches!(boundary, Boundary::OverriddenModuleDefault) {
                            "value = 'ok'"
                        } else {
                            ""
                        }
                    ),
                );
            } else if matches!(boundary, Boundary::OverriddenModuleDefault) {
                for source in generated.files.values_mut() {
                    *source = source.replace("unused = 'ok'", "unused = 'ok' value = 'ok'");
                }
            }
            String::new()
        }
        Boundary::Output => format!("attributes {{ out: String = {expression} }}\n"),
        Boundary::Export => format!("exports {{ out = {expression} }}\n"),
        Boundary::ProviderConfig => {
            generated.files.insert(
                "providers.crn".into(),
                format!("provider mock {{ endpoint = {expression} }}\n"),
            );
            String::new()
        }
    };
    let file = generated
        .files
        .get_mut(if module { "child/main.crn" } else { "main.crn" })
        .unwrap();
    *file = format!("{declaration}{file}");
    Ok(generated)
}

pub fn apply_fixture(
    position: Position,
    liveness: Liveness,
    quality: Quality,
    converted: bool,
) -> Result<Fixture, &'static str> {
    if matches!(position, Position::DataSource) {
        return Err(
            "the native mock has no Query read handler; validate and LSP still cover this typed input slot",
        );
    }
    if matches!(liveness, Liveness::ReadOnly) {
        return Err("apply is a validating entry, not the read-only entry");
    }
    Ok(fixture(position, liveness, quality, converted))
}

pub fn fixture(
    position: Position,
    liveness: Liveness,
    quality: Quality,
    converted: bool,
) -> Fixture {
    let mut files = BTreeMap::from([
        ("providers.crn".into(), "provider mock {}\n".into()),
        (
            "backend.crn".into(),
            "backend local { path = 'carina.state.json' }\n".into(),
        ),
    ]);
    let body = position.body(
        quality,
        converted,
        matches!(liveness, Liveness::DestinationUnknown),
    );
    let body = match liveness {
        Liveness::EmptyListFor
        | Liveness::EmptyIndexedFor
        | Liveness::EmptyMapFor
        | Liveness::UnselectedIf
        | Liveness::DeferredFor
        | Liveness::UnselectedModuleIf => body.replace("let query = read", "read"),
        Liveness::Live
        | Liveness::ModuleTemplate
        | Liveness::ModuleCallNotExpanded
        | Liveness::ModuleInstance
        | Liveness::DestinationUnknown
        | Liveness::OrdinaryStopped
        | Liveness::ReadOnly => body
            .replace("mock.Sink {", "let sink = mock.Sink {")
            .replace("mock.NotLoaded {", "let sink = mock.NotLoaded {"),
    };
    let main = match liveness {
        Liveness::Live | Liveness::DestinationUnknown | Liveness::ReadOnly => body,
        Liveness::EmptyListFor => format!("for item in [] {{\n{body}}}\n"),
        Liveness::EmptyIndexedFor => format!("for (index, item) in [] {{\n{body}}}\n"),
        Liveness::EmptyMapFor => {
            format!("let things = {{}}\nfor key, value in things {{\n{body}}}\n")
        }
        Liveness::UnselectedIf => format!("let on = false\nif on {{\n{body}}}\n"),
        Liveness::DeferredFor => {
            format!("let producer = mock.Source {{}}\nfor item in producer.items {{\n{body}}}\n")
        }
        Liveness::UnselectedModuleIf => {
            files.insert(
                "child/main.crn".into(),
                format!("arguments {{ on: Bool }}\nif on {{\n{body}}}\n"),
            );
            "let child = use { source = './child' }\nlet net = child { on = false }\n".into()
        }
        Liveness::ModuleTemplate => {
            files.insert(
                "child/main.crn".into(),
                format!("arguments {{ unused: String }}\n{body}"),
            );
            "let child = use { source = './child' }\n".into()
        }
        Liveness::ModuleCallNotExpanded => {
            files.insert(
                "child/main.crn".into(),
                format!("arguments {{ unused: String }}\n{body}"),
            );
            "let child = use { source = './child' }\nfor item in [] { child { unused = 'ok' } }\n"
                .into()
        }
        Liveness::ModuleInstance => {
            files.insert(
                "child/main.crn".into(),
                format!("arguments {{ unused: String }}\n{body}"),
            );
            "let child = use { source = './child' }\nlet net = child { unused = 'ok' }\n".into()
        }
        Liveness::OrdinaryStopped => {
            files.insert(
                "sibling.crn".into(),
                "let bad = mock.Sink { attr = missing.output }\n".into(),
            );
            body
        }
    };
    files.insert("main.crn".into(), main);
    if matches!(position, Position::DataSource) {
        for source in files.values_mut() {
            *source = source.replace("\nfor ", "\nlet queries = for ");
            if source.starts_with("for ") {
                *source = format!("let queries = {source}");
            }
            *source = source.replace("\nif on ", "\nlet queries = if on ");
        }
    }
    Fixture { files }
}

/// Generate deterministic no-`as` twins; this function does no validation.
pub fn emit_twins(directory: &Path) -> usize {
    let mut count = 0;
    let mut manifest = Vec::new();
    for position in POSITIONS {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                fixture(position, liveness, quality, false).write(
                    &directory
                        .join(position.label())
                        .join(liveness.label())
                        .join(quality.label()),
                );
                manifest.push(serde_json::json!({"position":position.label(),"liveness":liveness.label(),"quality":quality.label(),"live_destination":liveness.has_live_destination(),"excluded":resource_cell_exclusion(position,liveness)}));
                count += 1;
            }
        }
    }
    for boundary in BOUNDARIES {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                match boundary_fixture(boundary, liveness, quality, false) {
            Ok(fixture) => { fixture.write(&directory.join("boundary").join(boundary.label()).join(liveness.label()).join(quality.label())); count += 1; }
            Err(reason) => manifest.push(serde_json::json!({"boundary":boundary.label(),"liveness":liveness.label(),"quality":quality.label(),"excluded":reason})),
        }
            }
        }
    }
    for position in POSITIONS {
        for liveness in LIVENESS {
            for quality in QUALITIES {
                match apply_fixture(position, liveness, quality, false) {
            Ok(fixture) => { fixture.write(&directory.join("apply").join(position.label()).join(liveness.label()).join(quality.label())); count += 1; }
            Err(reason) => manifest.push(serde_json::json!({"run":"apply","position":position.label(),"liveness":liveness.label(),"quality":quality.label(),"excluded":reason})),
        }
            }
        }
    }
    std::fs::write(
        directory.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    count
}
